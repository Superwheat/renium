#import <AppKit/AppKit.h>
#import <objc/runtime.h>
#include <atomic>
#include <cstdlib>
#include <ctime>
#include <cstring>
#include <limits.h>
#include <mach-o/dyld.h>
#include <spawn.h>
#include <unistd.h>

namespace {
std::atomic<double> guardedUntil{0};
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
    if (now() >= guardedUntil.load(std::memory_order_relaxed))
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
    guardedUntil.store(now() + 90, std::memory_order_relaxed);
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
};
