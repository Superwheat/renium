#import <AppKit/AppKit.h>
#import <objc/runtime.h>
#include <atomic>
#include <mutex>
#include <cstdlib>
#include <ctime>
#include <cstring>
#include <limits.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <unistd.h>

namespace {
std::atomic<bool> guarded{false};
char enginePath[PATH_MAX] = {};
char launcherPath[PATH_MAX] = {};

void configurePlayArguments()
{
    // AppKit converts file-looking command arguments into open-document events.
    // A late event can overwrite Studio's StartServer task with EditFile. Qt
    // filters these duplicates only while its application is still launching.
    // These child tasks already parse -localProjectFile themselves; suppress
    // only AppKit's argument conversion, without changing real file-open events
    // or any persistent user preferences.
    @autoreleasepool {
        NSArray<NSString*>* arguments = NSProcessInfo.processInfo.arguments;
        for (NSUInteger index = 1; index + 1 < arguments.count; ++index)
        {
            if (([arguments[index] isEqualToString:@"-task"] ||
                 [arguments[index] isEqualToString:@"--task"]) &&
                ([arguments[index + 1] isEqualToString:@"StartServer"] ||
                 [arguments[index + 1] isEqualToString:@"StartClient"]))
            {
                NSUserDefaults* defaults = NSUserDefaults.standardUserDefaults;
                NSMutableDictionary* domain = [[defaults volatileDomainForName:NSArgumentDomain] mutableCopy];
                domain[@"NSTreatUnknownArgumentsAsOpen"] = @NO;
                [defaults setVolatileDomain:domain forName:NSArgumentDomain];
                [domain release];
                return;
            }
        }
    }
}

const char* childExecutable(const char* path)
{
    // QProcess uses the running image path (RobloxStudio.bin), bypassing our
    // signed launcher. Redirect only that exact image through its launcher so
    // Play inherits the same helper. No allocation/locks after fork or vfork.
    return enginePath[0] && std::strcmp(path, enginePath) == 0 ? launcherPath : path;
}

double now()
{
    struct timespec value{};
    clock_gettime(CLOCK_MONOTONIC, &value);
    return value.tv_sec + value.tv_nsec / 1e9;
}

bool backgroundRequest()
{
    // Like the Windows guard, this lasts for the process's lifetime: late
    // dialogs, Play windows and the engine's own focus grabs stay behind
    // until the user selects Studio themselves.
    if (!guarded.load(std::memory_order_relaxed))
        return false;
    NSRunningApplication* foreground = NSWorkspace.sharedWorkspace.frontmostApplication;
    if (!foreground || foreground.processIdentifier == getpid())
        return false;
    // AppKit's user activation remains intact (Dock, Cmd-Tab, clicking a
    // window). Only Studio's own unsolicited requests are suppressed.
    NSEvent* event = NSApp.currentEvent;
    if (event && now() - event.timestamp < 0.5)
        switch (event.type)
        {
            case NSEventTypeLeftMouseDown:
            case NSEventTypeRightMouseDown:
            case NSEventTypeKeyDown:
                return false;
            default:
                break;
        }
    return true;
}

// A Play session locks and hides the pointer through CoreGraphics, which acts
// on the whole session regardless of which application is active. Studio's
// own calls are honoured only while Studio is the active application; the
// requested state is remembered and applied when the user switches to Studio,
// and undone when they switch away.
std::mutex pointerLock;
bool pointerOwner = false;
bool associationRequested = true;
int hiddenDisplayCursor = 0;
int hiddenCursor = 0;
void (*originalHide)(id, SEL);
void (*originalUnhide)(id, SEL);

void applyPointerOwnership(bool owner)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    if (pointerOwner == owner) return;
    pointerOwner = owner;
    if (!associationRequested) CGAssociateMouseAndMouseCursorPosition(!owner);
    for (int count = hiddenDisplayCursor; count > 0; --count)
    {
        if (owner) CGDisplayHideCursor(kCGDirectMainDisplay);
        else CGDisplayShowCursor(kCGDirectMainDisplay);
    }
    for (int count = hiddenCursor; count > 0; --count)
    {
        if (owner) originalHide(NSCursor.class, @selector(hide));
        else originalUnhide(NSCursor.class, @selector(unhide));
    }
}

CGError pointerWarp(CGPoint point)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    return pointerOwner ? CGWarpMouseCursorPosition(point) : kCGErrorSuccess;
}
CGError pointerMove(CGDirectDisplayID display, CGPoint point)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    return pointerOwner ? CGDisplayMoveCursorToPoint(display, point) : kCGErrorSuccess;
}
CGError pointerAssociate(boolean_t connected)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    associationRequested = connected;
    return pointerOwner ? CGAssociateMouseAndMouseCursorPosition(connected) : kCGErrorSuccess;
}
CGError pointerHideDisplayCursor(CGDirectDisplayID display)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    ++hiddenDisplayCursor;
    return pointerOwner ? CGDisplayHideCursor(display) : kCGErrorSuccess;
}
CGError pointerShowDisplayCursor(CGDirectDisplayID display)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    if (hiddenDisplayCursor > 0) --hiddenDisplayCursor;
    return pointerOwner ? CGDisplayShowCursor(display) : kCGErrorSuccess;
}
void pointerHide(id cursorClass, SEL selector)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    ++hiddenCursor;
    if (pointerOwner) originalHide(cursorClass, selector);
}
void pointerUnhide(id cursorClass, SEL selector)
{
    std::lock_guard<std::mutex> guard(pointerLock);
    if (hiddenCursor > 0) --hiddenCursor;
    if (pointerOwner) originalUnhide(cursorClass, selector);
}

void installPointerGuard()
{
    originalHide = reinterpret_cast<void (*)(id, SEL)>(method_setImplementation(
        class_getClassMethod(NSCursor.class, @selector(hide)), reinterpret_cast<IMP>(pointerHide)));
    originalUnhide = reinterpret_cast<void (*)(id, SEL)>(method_setImplementation(
        class_getClassMethod(NSCursor.class, @selector(unhide)), reinterpret_cast<IMP>(pointerUnhide)));
    NSNotificationCenter* center = NSNotificationCenter.defaultCenter;
    [center addObserverForName:NSApplicationDidBecomeActiveNotification
                        object:nil
                         queue:nil
                    usingBlock:^(NSNotification*) {
                        // Studio's own requests were suppressed, so the
                        // application became active through the user (Dock,
                        // Cmd-Tab, a click) or the system: the guard is over.
                        guarded.store(false, std::memory_order_relaxed);
                        applyPointerOwnership(true);
                    }];
    [center addObserverForName:NSApplicationWillResignActiveNotification
                        object:nil
                         queue:nil
                    usingBlock:^(NSNotification*) { applyPointerOwnership(false); }];
}

using Activate = void (*)(id, SEL, BOOL);
using RunningActivate = BOOL (*)(id, SEL, NSApplicationActivationOptions);
using WindowAction = void (*)(id, SEL, id);
using WindowOrder = void (*)(id, SEL, NSWindowOrderingMode, NSInteger);
Activate originalActivate;
RunningActivate originalRunningActivate;
WindowAction originalKeyFront;
void (*originalFrontRegardless)(id, SEL);
WindowOrder originalOrder;

void activate(id app, SEL selector, BOOL ignoreOthers)
{
    if (!backgroundRequest()) originalActivate(app, selector, ignoreOthers);
}
BOOL runningActivate(id app, SEL selector, NSApplicationActivationOptions options)
{
    if ([app processIdentifier] == getpid() && backgroundRequest()) return NO;
    return originalRunningActivate(app, selector, options);
}
void order(id window, SEL selector, NSWindowOrderingMode mode, NSInteger relative)
{
    if (mode == NSWindowAbove && backgroundRequest())
    {
        mode = NSWindowBelow;
        relative = 0;
    }
    originalOrder(window, selector, mode, relative);
}
void keyFront(id window, SEL selector, id sender)
{
    if (backgroundRequest())
        originalOrder(window, @selector(orderWindow:relativeTo:), NSWindowBelow, 0);
    else originalKeyFront(window, selector, sender);
}
void frontRegardless(id window, SEL selector)
{
    if (backgroundRequest())
        originalOrder(window, @selector(orderWindow:relativeTo:), NSWindowBelow, 0);
    else originalFrontRegardless(window, selector);
}
}

extern "C" bool ReniumArmLaunchGuard()
{
    // Inherited only by children of this exact Studio. A manually opened,
    // unrelated Studio never receives this opt-in.
    if (setenv("RENIUM_BACKGROUND_LAUNCH", "1", 1) != 0) return false;
    guarded.store(true, std::memory_order_relaxed);
    return true;
}

extern "C" void ReniumInitializeLaunchGuard()
{
    configurePlayArguments();
    uint32_t size = sizeof(enginePath);
    if (_NSGetExecutablePath(enginePath, &size) == 0)
    {
        const size_t length = std::strlen(enginePath);
        if (length > 4 && std::strcmp(enginePath + length - 4, ".bin") == 0)
        {
            std::memcpy(launcherPath, enginePath, length - 4);
            launcherPath[length - 4] = '\0';
            if (access(launcherPath, X_OK) != 0) enginePath[0] = '\0';
        }
        else enginePath[0] = '\0';
    }
    else enginePath[0] = '\0';
    originalActivate = reinterpret_cast<Activate>(method_setImplementation(
        class_getInstanceMethod(NSApplication.class, @selector(activateIgnoringOtherApps:)),
        reinterpret_cast<IMP>(activate)));
    originalRunningActivate = reinterpret_cast<RunningActivate>(method_setImplementation(
        class_getInstanceMethod(NSRunningApplication.class, @selector(activateWithOptions:)),
        reinterpret_cast<IMP>(runningActivate)));
    originalOrder = reinterpret_cast<WindowOrder>(method_setImplementation(
        class_getInstanceMethod(NSWindow.class, @selector(orderWindow:relativeTo:)),
        reinterpret_cast<IMP>(order)));
    originalKeyFront = reinterpret_cast<WindowAction>(method_setImplementation(
        class_getInstanceMethod(NSWindow.class, @selector(makeKeyAndOrderFront:)),
        reinterpret_cast<IMP>(keyFront)));
    originalFrontRegardless = reinterpret_cast<decltype(originalFrontRegardless)>(method_setImplementation(
        class_getInstanceMethod(NSWindow.class, @selector(orderFrontRegardless)),
        reinterpret_cast<IMP>(frontRegardless)));
    installPointerGuard();
    if (const char* enabled = getenv("RENIUM_BACKGROUND_LAUNCH"); enabled && *enabled == '1')
        ReniumArmLaunchGuard();
}

static int launchExecv(const char* path, char* const args[])
{
    return execv(childExecutable(path), args);
}
static int launchExecve(const char* path, char* const args[], char* const environment[])
{
    return execve(childExecutable(path), args, environment);
}
static int launchSpawn(pid_t* pid, const char* path, const posix_spawn_file_actions_t* actions,
    const posix_spawnattr_t* attributes, char* const args[], char* const environment[])
{
    return posix_spawn(pid, childExecutable(path), actions, attributes, args, environment);
}

// dyld interposition applies to callers outside this image; calls above reach
// libc itself. It requires no writable executable pages or instruction patch.
__attribute__((used, section("__DATA,__interpose")))
static const struct { const void* replacement; const void* original; } spawnInterposes[] = {
    { reinterpret_cast<const void*>(launchExecv), reinterpret_cast<const void*>(execv) },
    { reinterpret_cast<const void*>(launchExecve), reinterpret_cast<const void*>(execve) },
    { reinterpret_cast<const void*>(launchSpawn), reinterpret_cast<const void*>(posix_spawn) },
    { reinterpret_cast<const void*>(pointerWarp), reinterpret_cast<const void*>(CGWarpMouseCursorPosition) },
    { reinterpret_cast<const void*>(pointerMove), reinterpret_cast<const void*>(CGDisplayMoveCursorToPoint) },
    { reinterpret_cast<const void*>(pointerAssociate), reinterpret_cast<const void*>(CGAssociateMouseAndMouseCursorPosition) },
    { reinterpret_cast<const void*>(pointerHideDisplayCursor), reinterpret_cast<const void*>(CGDisplayHideCursor) },
    { reinterpret_cast<const void*>(pointerShowDisplayCursor), reinterpret_cast<const void*>(CGDisplayShowCursor) },
};
