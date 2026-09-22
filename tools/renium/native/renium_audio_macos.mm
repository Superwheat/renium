#import <AppKit/AppKit.h>
#include <CoreAudio/CoreAudio.h>
#include <chrono>
#include <cstdio>
#include <map>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>
#include <unistd.h>

namespace {
using Clock = std::chrono::steady_clock;
struct AudioState {
    std::mutex mutex;
    std::map<std::string, bool> changed;
    Clock::time_point lease;
    bool watching = false;
};

AudioState& State() {
    static auto* state = new AudioState;
    return *state;
}

void Check(OSStatus result, const char* operation) {
    if (result != noErr) throw std::runtime_error(std::string(operation) + " (Core Audio " + std::to_string(result) + ")");
}

template<class T> T Read(AudioObjectID object, AudioObjectPropertySelector selector, AudioObjectPropertyScope scope = kAudioObjectPropertyScopeGlobal) {
    AudioObjectPropertyAddress address{selector, scope, kAudioObjectPropertyElementMain};
    T result{};
    UInt32 size = sizeof(result);
    Check(AudioObjectGetPropertyData(object, &address, 0, nullptr, &size, &result), "Could not read process audio state");
    return result;
}

std::string Identity(AudioObjectID device) {
    auto value = Read<CFStringRef>(device, kAudioDevicePropertyDeviceUID);
    char text[1024]{};
    const bool valid = CFStringGetCString(value, text, sizeof(text), kCFStringEncodingUTF8);
    CFRelease(value);
    if (!valid) throw std::runtime_error("Invalid audio device identity");
    return text;
}

void Apply(unsigned action, unsigned& count, unsigned& muted, unsigned& pending, bool& focused) {
    focused = [NSWorkspace sharedWorkspace].frontmostApplication.processIdentifier == getpid();
    const bool mute = action == 2 || (action == 4 && !focused);
    auto& state = State();
    AudioObjectPropertyAddress list{kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    Check(AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &list, 0, nullptr, &size), "Could not list output devices");
    std::vector<AudioDeviceID> devices(size / sizeof(AudioDeviceID));
    Check(AudioObjectGetPropertyData(kAudioObjectSystemObject, &list, 0, nullptr, &size, devices.data()), "Could not list output devices");
    std::string error;
    for (auto device : devices) {
        try {
            AudioObjectPropertyAddress streams{kAudioDevicePropertyStreams, kAudioDevicePropertyScopeOutput, kAudioObjectPropertyElementMain};
            UInt32 streamSize = 0;
            Check(AudioObjectGetPropertyDataSize(device, &streams, 0, nullptr, &streamSize), "Could not inspect output device");
            if (!streamSize) continue;
            const auto key = Identity(device);
            AudioObjectPropertyAddress address{kAudioDevicePropertyProcessMute, kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
            if (!AudioObjectHasProperty(device, &address)) {
                if (action != 0 && action != 1) error = "An output device does not support per-process mute (aggregate/virtual devices may be unsupported)";
                continue;
            }
            bool current = Read<UInt32>(device, kAudioDevicePropertyProcessMute, kAudioObjectPropertyScopeOutput) != 0;
            const bool owned = state.changed.count(key) != 0;
            const bool unmute = action == 3 || (action == 4 && focused);
            const bool desired = unmute ? false : mute ? true : owned ? false : current;
            if (action != 0 && desired != current) {
                UInt32 value = desired;
                Check(AudioObjectSetPropertyData(device, &address, 0, nullptr, sizeof(value), &value), "Could not change process mute");
                if ((Read<UInt32>(device, kAudioDevicePropertyProcessMute, kAudioObjectPropertyScopeOutput) != 0) != desired)
                    throw std::runtime_error("Output device did not retain process mute");
                if (desired) state.changed[key] = true;
                current = desired;
            }
            if (action != 0 && !mute) state.changed.erase(key);
            ++count;
            muted += current;
        } catch (const std::exception& failure) { error = failure.what(); }
    }
    pending = static_cast<unsigned>(state.changed.size());
    if (!error.empty()) throw std::runtime_error(error);
}

void WatchLease() {
    for (;;) {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
        @autoreleasepool {
            auto& state = State();
            std::lock_guard<std::mutex> lock(state.mutex);
            if (state.changed.empty() || Clock::now() - state.lease < std::chrono::seconds(3)) continue;
            unsigned count = 0, muted = 0, pending = 0;
            bool focused = false;
            try { Apply(1, count, muted, pending, focused); } catch (...) {}
        }
    }
}
}

extern "C" bool ReniumStudioAudio(unsigned action, unsigned* count, unsigned* muted, unsigned* pending, bool* focused, char* error, std::size_t errorSize) {
    @autoreleasepool {
        auto& state = State();
        std::lock_guard<std::mutex> lock(state.mutex);
        try {
            if (action > 4) throw std::runtime_error("Invalid audio action");
            if (action != 0) state.lease = Clock::now();
            if ((action == 2 || action == 4) && !state.watching) {
                std::thread(WatchLease).detach();
                state.watching = true;
            }
            Apply(action, *count, *muted, *pending, *focused);
            return true;
        } catch (const std::exception& failure) {
            std::snprintf(error, errorSize, "%s", failure.what());
            *pending = static_cast<unsigned>(state.changed.size());
            return false;
        }
    }
}
