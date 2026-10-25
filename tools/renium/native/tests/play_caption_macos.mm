#include "../renium_play_caption_macos.mm"
#include <cassert>
#include <chrono>
#include <future>
#include <iostream>

int main() {
    @autoreleasepool {
        [NSApplication sharedApplication];
        auto window = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, 320, 240)
            styleMask:NSWindowStyleMaskTitled backing:NSBackingStoreBuffered defer:NO];
        window.title = @"Source";
        Request request{};
        request.version = 1; request.size = sizeof(request); request.action = 2;
        request.window = static_cast<std::uint32_t>(window.windowNumber);
        request.token = 42; request.hostPid = getpid();
        proc_bsdinfo info{};
        assert(proc_pidinfo(getpid(), PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info));
        request.hostSeconds = info.pbi_start_tvsec; request.hostMicros = info.pbi_start_tvusec;
        std::strcpy(request.ordinary, "Source");
        handle(request);
        const auto send = [&] {
            auto future = std::async(std::launch::async, [request] {
                std::vector<unsigned char> output;
                ReniumTeleportCaption(std::string(reinterpret_cast<const char*>(&request), sizeof(request)), output);
                assert(output.size() == sizeof(request));
            });
            // The UI thread is deliberately occupied during the transport call.
            assert(future.wait_for(std::chrono::milliseconds(250)) == std::future_status::ready);
            future.get();
        };
        request.action = 3;
        std::strcpy(request.next, "Source - Teleporting to Destination");
        send(); drain();
        assert([window.title isEqualToString:@"Source - Teleporting to Destination"]);
        window.title = @"Studio rewrote the document";
        apply(current);
        assert([window.title isEqualToString:@"Studio rewrote the document - Teleporting to Destination"]);
        std::strcpy(request.next, "Source - Teleported to Destination");
        send();
        request.action = 4;
        send(); drain();
        assert(!current && !ownerToken);
        assert([window.title isEqualToString:@"Studio rewrote the document"]);
        request.action = 3;
        bool rejected = false;
        try { send(); } catch (const std::exception&) { rejected = true; }
        assert(rejected);
        [window release];
    }
    std::cout << "Mac caption: asynchronous update, engine rewrite, Stop, stale owner passed\n";
}
