#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <os/lock.h>
#import <signal.h>
#import <stdatomic.h>
#import <unistd.h>

static pid_t target_pid;
static pid_t parent_pid;
static CGWindowID target_window;
static NSPanel *panel;
static atomic_bool shield_active;
static CGRect target_rect;
static os_unfair_lock target_rect_lock = OS_UNFAIR_LOCK_INIT;
static volatile sig_atomic_t stopping;
static const int64_t event_marker = 0x52454e49554d;

@interface ReniumShieldView : NSView
@property(nonatomic, copy) NSString *label;
@end

@implementation ReniumShieldView
- (void)drawRect:(NSRect)dirtyRect {
    [super drawRect:dirtyRect];
    NSColor *orange = [NSColor colorWithRed:245.0 / 255.0 green:158.0 / 255.0 blue:11.0 / 255.0 alpha:1.0];
    [orange setStroke];
    NSBezierPath *border = [NSBezierPath bezierPathWithRect:NSInsetRect(self.bounds, 1.5, 1.5)];
    border.lineWidth = 3.0;
    [border stroke];
    NSDictionary *attributes = @{
        NSForegroundColorAttributeName: orange,
        NSFontAttributeName: [NSFont monospacedSystemFontOfSize:14.0 weight:NSFontWeightMedium]
    };
    NSSize size = [self.label sizeWithAttributes:attributes];
    [self.label drawAtPoint:NSMakePoint(NSWidth(self.bounds) - size.width - 10.0, NSHeight(self.bounds) - size.height - 10.0)
             withAttributes:attributes];
}
@end

static bool read_target_bounds(CGRect *bounds) {
    CFArrayRef windows = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, target_window);
    if (windows == NULL || CFArrayGetCount(windows) == 0) {
        if (windows != NULL) CFRelease(windows);
        return false;
    }
    CFDictionaryRef window = CFArrayGetValueAtIndex(windows, 0);
    CFNumberRef owner = CFDictionaryGetValue(window, kCGWindowOwnerPID);
    pid_t owner_pid = 0;
    bool valid = owner != NULL && CFNumberGetValue(owner, kCFNumberIntType, &owner_pid) && owner_pid == target_pid;
    CFDictionaryRef encoded = CFDictionaryGetValue(window, kCGWindowBounds);
    valid = valid && encoded != NULL && CGRectMakeWithDictionaryRepresentation(encoded, bounds);
    CFRelease(windows);
    return valid && bounds->size.width > 0.0 && bounds->size.height > 0.0;
}

static CGEventRef filter_event(CGEventTapProxy proxy, CGEventType type, CGEventRef event, void *context) {
    (void)proxy;
    (void)context;
    if (!atomic_load_explicit(&shield_active, memory_order_acquire)) return event;
    if (CGEventGetIntegerValueField(event, kCGEventSourceUserData) == event_marker) return event;
    if (type == kCGEventKeyDown || type == kCGEventKeyUp) {
        int64_t key = CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode);
        CGEventFlags flags = CGEventGetFlags(event);
        return key == 48 && (flags & kCGEventFlagMaskCommand) != 0 ? event : NULL;
    }
    if (type == kCGEventLeftMouseDown || type == kCGEventLeftMouseUp ||
        type == kCGEventRightMouseDown || type == kCGEventRightMouseUp ||
        type == kCGEventOtherMouseDown || type == kCGEventOtherMouseUp ||
        type == kCGEventScrollWheel) {
        CGPoint point = CGEventGetLocation(event);
        os_unfair_lock_lock(&target_rect_lock);
        CGRect bounds = target_rect;
        os_unfair_lock_unlock(&target_rect_lock);
        if (CGRectContainsPoint(bounds, point)) return NULL;
    }
    return event;
}

static void update_shield(CFRunLoopTimerRef timer, void *context) {
    (void)timer;
    (void)context;
    if (stopping || kill(parent_pid, 0) != 0) {
        [NSApp terminate:nil];
        return;
    }
    CGRect bounds;
    NSRunningApplication *frontmost = NSWorkspace.sharedWorkspace.frontmostApplication;
    bool visible = frontmost.processIdentifier == target_pid && read_target_bounds(&bounds);
    atomic_store_explicit(&shield_active, visible, memory_order_release);
    if (!visible) {
        [panel orderOut:nil];
        return;
    }
    os_unfair_lock_lock(&target_rect_lock);
    target_rect = bounds;
    os_unfair_lock_unlock(&target_rect_lock);
    CGRect main = CGDisplayBounds(CGMainDisplayID());
    NSRect frame = NSMakeRect(bounds.origin.x, main.size.height - CGRectGetMaxY(bounds), bounds.size.width, bounds.size.height);
    [panel setFrame:frame display:YES];
    [panel orderWindow:NSWindowAbove relativeTo:(NSInteger)target_window];
}

static void stop_handler(int signal_number) {
    (void)signal_number;
    stopping = 1;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 5) return 2;
        target_pid = (pid_t)strtol(argv[1], NULL, 10);
        target_window = (CGWindowID)strtoul(argv[2], NULL, 10);
        parent_pid = (pid_t)strtol(argv[3], NULL, 10);
        if (target_pid <= 0 || target_window == 0 || parent_pid <= 0) return 2;
        signal(SIGTERM, stop_handler);
        signal(SIGINT, stop_handler);
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyProhibited];
        panel = [[NSPanel alloc] initWithContentRect:NSMakeRect(0, 0, 1, 1)
                                           styleMask:NSWindowStyleMaskBorderless
                                             backing:NSBackingStoreBuffered
                                               defer:NO];
        panel.opaque = NO;
        panel.backgroundColor = NSColor.clearColor;
        panel.hasShadow = NO;
        panel.ignoresMouseEvents = NO;
        panel.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorFullScreenAuxiliary;
        ReniumShieldView *view = [[ReniumShieldView alloc] initWithFrame:panel.contentView.bounds];
        view.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
        view.label = [NSString stringWithUTF8String:argv[4]];
        panel.contentView = view;
        CGEventMask mask = CGEventMaskBit(kCGEventLeftMouseDown) | CGEventMaskBit(kCGEventLeftMouseUp) |
            CGEventMaskBit(kCGEventRightMouseDown) | CGEventMaskBit(kCGEventRightMouseUp) |
            CGEventMaskBit(kCGEventOtherMouseDown) | CGEventMaskBit(kCGEventOtherMouseUp) |
            CGEventMaskBit(kCGEventScrollWheel) | CGEventMaskBit(kCGEventKeyDown) | CGEventMaskBit(kCGEventKeyUp);
        CFMachPortRef tap = CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, kCGEventTapOptionDefault, mask, filter_event, NULL);
        if (tap == NULL) {
            fputs("Accessibility permission is required\n", stderr);
            return 3;
        }
        CFRunLoopSourceRef source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0);
        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopCommonModes);
        CFRunLoopTimerContext timer_context = {0};
        CFRunLoopTimerRef timer = CFRunLoopTimerCreate(kCFAllocatorDefault, CFAbsoluteTimeGetCurrent(), 0.016, 0, 0, update_shield, &timer_context);
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes);
        puts("ready");
        fflush(stdout);
        [NSApp run];
        CFRunLoopTimerInvalidate(timer);
        CFRunLoopSourceInvalidate(source);
        CFRelease(timer);
        CFRelease(source);
        CFRelease(tap);
    }
    return 0;
}
