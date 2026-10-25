#import <AppKit/AppKit.h>
#include <libproc.h>
#include <sys/proc_info.h>
#include <cstdint>
#include <cstring>
#include <memory>
#include <mutex>
#include <optional>
#include <stdexcept>
#include <string>
#include <vector>
#include <atomic>
#include <dlfcn.h>

namespace {
struct QtObjects {
    struct Data { std::atomic<int> refs; int allocated, begin, end; void* items[1]; };
    Data* data = static_cast<Data*>(dlsym(RTLD_DEFAULT, "_ZN9QListData11shared_nullE"));
    ~QtObjects() {
        if (data && data->refs.load() != -1 && data->refs.fetch_sub(1) == 1) {
            const auto dispose = reinterpret_cast<void(*)(void*)>(dlsym(RTLD_DEFAULT, "_ZN9QListData7disposeEPNS_4DataE"));
            if (dispose) dispose(data);
        }
    }
    Data* checked() const {
        if (!data || data->begin < 0 || data->end < data->begin || data->end > data->allocated || data->allocated > 4096)
            throw std::runtime_error("Qt object list changed");
        return data;
    }
};
struct QtText {
    void* data = nullptr;
    ~QtText() {
        if (data) {
            auto refs = static_cast<std::atomic<int>*>(data);
            if (refs->load() != -1 && refs->fetch_sub(1) == 1) {
                const auto dispose = reinterpret_cast<void(*)(void*, std::size_t, std::size_t)>(
                    dlsym(RTLD_DEFAULT, "_ZN10QArrayData10deallocateEPS_mm"));
                if (dispose) dispose(data, 2, 8);
            }
        }
    }
};
struct Request {
    std::uint32_t version, size, action, window;
    std::uint64_t token;
    std::uint32_t hostPid, reserved;
    std::uint64_t hostSeconds, hostMicros;
    char ordinary[1024], next[1024];
};
static_assert(sizeof(Request) == 2096);
struct Caption {
    Request request{};
    NSWindow* window = nil;
    NSString* ordinary = nil;
    NSString* written = nil;
    NSString* suffix = nil;
    ~Caption() { [window release]; [ordinary release]; [written release]; [suffix release]; }
};
std::shared_ptr<Caption> current;
std::mutex pendingMutex;
std::optional<Request> pending;
std::uint64_t ownerToken = 0;
bool hostAlive(const Request& r) {
    proc_bsdinfo info{};
    return proc_pidinfo(r.hostPid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info)
        && info.pbi_uid == getuid() && info.pbi_start_tvsec == r.hostSeconds && info.pbi_start_tvusec == r.hostMicros;
}
void restore(const std::shared_ptr<Caption>& caption) {
    if (caption->written && [caption->window.title isEqualToString:caption->written])
        caption->window.title = caption->ordinary;
    if (current == caption) current.reset();
    std::lock_guard lock(pendingMutex);
    if (ownerToken == caption->request.token) {
        ownerToken = 0;
        pending.reset();
    }
}
void apply(const std::shared_ptr<Caption>& caption) {
    NSString* actual = caption->window.title;
    if (!actual.length) return;
    if (![actual isEqualToString:caption->written]) {
        [caption->ordinary release]; caption->ordinary = [actual copy];
    }
    NSString* next = [caption->ordinary stringByAppendingString:caption->suffix ?: @""];
    if (![actual isEqualToString:next]) caption->window.title = next;
    [caption->written release]; caption->written = [next copy];
}
void watch(const std::shared_ptr<Caption>& caption) {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 250 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
        if (current != caption) return;
        if (!hostAlive(caption->request) || ![NSApp.windows containsObject:caption->window]) restore(caption);
        else { apply(caption); watch(caption); }
    });
}
NSWindow* window(std::uint32_t number) {
    if (!number && current) number = current->request.window;
    NSWindow* result = nil;
    for (NSWindow* candidate in NSApp.windows) {
        if (number ? candidate.windowNumber != number :
            (!candidate.visible || candidate.level != NSNormalWindowLevel || [candidate isKindOfClass:NSPanel.class]
                || !(candidate.styleMask & NSWindowStyleMaskTitled) || !(candidate.styleMask & NSWindowStyleMaskResizable)
                || !candidate.title.length)) continue;
        if (result) throw std::runtime_error("Teleport caption has multiple Studio windows");
        result = candidate;
    }
    if (!result) throw std::runtime_error("Teleport caption window ended");
    return result;
}
void handle(Request& r) {
    if (r.version != 1 || r.size != sizeof(r) || r.action < 1 || r.action > 4 || r.reserved)
        throw std::runtime_error("Incompatible Mac teleport caption request");
    if (r.action == 1) {
        const auto selected = window(r.window);
        const char* title = selected.title.UTF8String;
        if (!title || std::strlen(title) >= sizeof(r.ordinary)) throw std::runtime_error("Studio title exceeds caption bound");
        r.window = static_cast<std::uint32_t>(selected.windowNumber);
        std::strcpy(r.ordinary, title);
        return;
    }
    if (!r.token || !r.window || !hostAlive(r) || r.ordinary[1023] || r.next[1023])
        throw std::runtime_error("Invalid Mac teleport caption ownership");
    if (r.action == 2) {
        if (current) throw std::runtime_error("Studio caption already has an owner");
        auto caption = std::make_shared<Caption>();
        caption->window = [window(r.window) retain];
        caption->ordinary = [[NSString alloc] initWithUTF8String:r.ordinary];
        if (!caption->ordinary || ![caption->window.title isEqualToString:caption->ordinary])
            throw std::runtime_error("Studio title changed before caption ownership");
        caption->request = r;
        {
            std::lock_guard lock(pendingMutex);
            ownerToken = r.token;
        }
        current = caption;
        watch(caption);
        return;
    }
    const auto caption = current;
    if (!caption || caption->request.token != r.token || caption->request.window != r.window)
        throw std::runtime_error("Mac teleport caption owner changed");
    if (r.action == 4) { restore(caption); return; }
    NSString* ordinary = [NSString stringWithUTF8String:r.ordinary];
    NSString* next = r.next[0] ? [NSString stringWithUTF8String:r.next] : ordinary;
    if (!ordinary || !next || ![next hasPrefix:ordinary])
        throw std::runtime_error("Invalid Studio caption suffix");
    [caption->suffix release]; caption->suffix = [[next substringFromIndex:ordinary.length] copy];
    apply(caption);
}

void drain() {
    std::optional<Request> request;
    {
        std::lock_guard lock(pendingMutex);
        request.swap(pending);
    }
    if (!request) return;
    @autoreleasepool {
        try { handle(*request); }
        catch (const std::exception& error) {
            NSLog(@"Renium teleport caption: %s", error.what());
        }
    }
}
}

// Resolve Qt's existing top-level widget to the Cocoa window already owned by
// the caption lease. Qt 5's public list/QString ABI is independent of Studio.
void* ReniumTeleportClientWidget(std::uint32_t number) {
    __block void* selected = nullptr;
    __block std::string error;
    auto find = ^{
        try {
            auto top = reinterpret_cast<QtObjects(*)()>(dlsym(RTLD_DEFAULT, "_ZN12QApplication15topLevelWidgetsEv"));
            auto winId = reinterpret_cast<void*(*)(void*)>(dlsym(RTLD_DEFAULT, "_ZNK7QWidget5winIdEv"));
            if (!top || !winId) throw std::runtime_error("Qt window lookup is unavailable");
            const auto widgets = top();
            auto data = widgets.checked();
            for (int i = data->begin; i < data->end; ++i) {
                auto widget = data->items[i];
                auto view = static_cast<NSView*>(winId(widget));
                if (view.window.windowNumber == number) {
                    if (selected) throw std::runtime_error("Client has duplicate native windows");
                    selected = widget;
                }
            }
        } catch (const std::exception& failure) { error = failure.what(); }
    };
    if ([NSThread isMainThread]) find(); else dispatch_sync(dispatch_get_main_queue(), find);
    if (!error.empty()) throw std::runtime_error(error);
    return selected;
}

void ReniumStudioAction(const std::string& payload, std::vector<unsigned char>& output) {
    struct Action { std::uint32_t version, size, window, found; char name[96]; };
    static_assert(sizeof(Action) == 112);
    if (payload.size() != sizeof(Action)) throw std::runtime_error("Truncated Mac Studio action");
    __block Action r{};
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.version != 1 || r.size != sizeof(r) || !r.window || !r.name[0] || r.name[95])
        throw std::runtime_error("Invalid Mac Studio action");
    __block std::string error;
    dispatch_sync(dispatch_get_main_queue(), ^{
        try {
            auto root = ReniumTeleportClientWidget(r.window);
            auto text = reinterpret_cast<QtText(*)(const char*, int)>(dlsym(RTLD_DEFAULT, "_ZN7QString15fromUtf8_helperEPKci"));
            auto find = reinterpret_cast<void(*)(const void*, const QtText&, const void*, QtObjects*, int)>(
                dlsym(RTLD_DEFAULT, "_Z23qt_qFindChildren_helperPK7QObjectRK7QStringRK11QMetaObjectP5QListIPvE6QFlagsIN2Qt15FindChildOptionEE"));
            auto meta = dlsym(RTLD_DEFAULT, "_ZN7QAction16staticMetaObjectE");
            auto enabled = reinterpret_cast<bool(*)(const void*)>(dlsym(RTLD_DEFAULT, "_ZNK7QAction9isEnabledEv"));
            auto activate = reinterpret_cast<void(*)(void*, int)>(dlsym(RTLD_DEFAULT, "_ZN7QAction8activateENS_11ActionEventE"));
            QtObjects actions;
            if (!root || !text || !find || !meta || !enabled || !activate || !actions.data)
                throw std::runtime_error("Qt named Studio action lookup is unavailable");
            const auto name = text(r.name, static_cast<int>(std::strlen(r.name)));
            find(root, name, meta, &actions, 1); // Qt::FindChildrenRecursively
            const auto data = actions.checked();
            r.found = static_cast<std::uint32_t>(data->end - data->begin);
            if (r.found != 1) throw std::runtime_error("Studio action does not have one window owner");
            auto action = data->items[data->begin];
            if (!enabled(action)) throw std::runtime_error("Studio action is disabled right now");
            activate(action, 0); // QAction::Trigger
        } catch (const std::exception& failure) { error = failure.what(); }
    });
    if (!error.empty()) throw std::runtime_error(error);
    output.resize(sizeof(r)); std::memcpy(output.data(), &r, sizeof(r));
}

// AppKit work is restricted to this process's existing window on its UI thread.
// No activation, ordering, resizing, or Accessibility permission is involved.
void ReniumTeleportCaption(const std::string& payload, std::vector<unsigned char>& output) {
    if (payload.size() != sizeof(Request)) throw std::runtime_error("Truncated Mac caption request");
    __block Request r{};
    std::memcpy(&r, payload.data(), sizeof(r));
    if (r.action == 3 || r.action == 4) {
        if (r.version != 1 || r.size != sizeof(r) || r.reserved || !r.token || !r.window
            || !hostAlive(r) || r.ordinary[1023] || r.next[1023])
            throw std::runtime_error("Invalid Mac teleport caption ownership");
        {
            std::lock_guard lock(pendingMutex);
            if (ownerToken != r.token) throw std::runtime_error("Mac teleport caption owner changed");
            pending = r;
            if (r.action == 4) ownerToken = 0;
        }
        // Coalesce title changes without waiting behind Studio's document load.
        // Stop is ordered on the same UI queue and invalidates further writes.
        dispatch_async(dispatch_get_main_queue(), ^{ drain(); });
        output.resize(sizeof(r));
        std::memcpy(output.data(), &r, sizeof(r));
        return;
    }
    __block std::string error;
    dispatch_sync(dispatch_get_main_queue(), ^{
        @autoreleasepool {
            try { handle(r); } catch (const std::exception& e) { error = e.what(); }
        }
    });
    if (!error.empty()) throw std::runtime_error(error);
    output.resize(sizeof(r));
    std::memcpy(output.data(), &r, sizeof(r));
}
