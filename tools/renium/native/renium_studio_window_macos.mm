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
NSWindow* window(std::uint32_t number) {
    NSWindow* result = nil;
    for (NSWindow* candidate in NSApp.windows) {
        if (number ? candidate.windowNumber != number :
            (!candidate.visible || candidate.level != NSNormalWindowLevel || [candidate isKindOfClass:NSPanel.class]
                || !(candidate.styleMask & NSWindowStyleMaskTitled) || !(candidate.styleMask & NSWindowStyleMaskResizable)
                || !candidate.title.length)) continue;
        if (result) throw std::runtime_error("Studio has multiple document windows");
        result = candidate;
    }
    if (!result) throw std::runtime_error("Studio document window ended");
    return result;
}
void handle(Request& r) {
    if (r.version != 1 || r.size != sizeof(r) || r.action != 1 || r.reserved)
        throw std::runtime_error("Incompatible Mac Studio window request");
    const auto selected = window(r.window);
    const char* title = selected.title.UTF8String;
    if (!title || std::strlen(title) >= sizeof(r.ordinary)) throw std::runtime_error("Studio title exceeds its bound");
    r.window = static_cast<std::uint32_t>(selected.windowNumber);
    std::strcpy(r.ordinary, title);
}
}


// Resolve Qt's existing top-level widget to the Cocoa window already owned by
// the daemon. Qt 5's public list/QString ABI is independent of Studio.
void* ReniumStudioWindowWidget(std::uint32_t number) {
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
            auto root = ReniumStudioWindowWidget(r.window);
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

// AppKit work is restricted to reading this process's existing window on its
// UI thread. No activation, ordering, resizing, or Accessibility permission is involved.
void ReniumStudioWindow(const std::string& payload, std::vector<unsigned char>& output) {
    if (payload.size() != sizeof(Request)) throw std::runtime_error("Truncated Mac Studio window request");
    __block Request r{};
    std::memcpy(&r, payload.data(), sizeof(r));
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
