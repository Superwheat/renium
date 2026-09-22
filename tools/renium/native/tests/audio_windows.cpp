#include "../renium_audio_windows.cpp"
#include <cassert>

namespace {
IAudioRenderClient* received = nullptr;
UINT32 frameCount = 0;
DWORD receivedFlags = 0;
HRESULT STDMETHODCALLTYPE Original(IAudioRenderClient* client, UINT32 frames, DWORD flags) {
    received = client;
    frameCount = frames;
    receivedFlags = flags;
    return S_FALSE;
}
}

int main() {
    assert(!renium_audio::Muted(4, 5000, 1000, true));
    assert(renium_audio::Muted(4, 5000, 1000, false));
    assert(renium_audio::Muted(2, 5000, 1000, true));
    assert(!renium_audio::Muted(1, 5000, 1000, false));
    assert(!renium_audio::Muted(4, 5000, 5000, false));
    IAudioRenderClientVtbl table{};
    table.ReleaseBuffer = Original;
    IAudioRenderClient client{&table};
    Install(&client);
    Install(&client);
    assert(GetState().count == 1);
    for (unsigned iteration = 0; iteration < 1000; ++iteration) {
        auto& state = GetState();
        state.deadline = GetTickCount64() + 3000;
        state.mode = 2;
        assert(IAudioRenderClient_ReleaseBuffer(&client, 128, 0) == S_FALSE);
        assert(received == &client && frameCount == 128 && receivedFlags == AUDCLNT_BUFFERFLAGS_SILENT);
        state.mode = 1;
        assert(IAudioRenderClient_ReleaseBuffer(&client, 96, 0) == S_FALSE);
        assert(frameCount == 96 && receivedFlags == 0);
        state.mode = 2;
        state.deadline = 0;
        assert(IAudioRenderClient_ReleaseBuffer(&client, 48, 0) == S_FALSE);
        assert(frameCount == 48 && receivedFlags == 0);
        assert(IAudioRenderClient_ReleaseBuffer(&client, 0, AUDCLNT_BUFFERFLAGS_SILENT) == S_FALSE);
        assert(frameCount == 0 && receivedFlags == AUDCLNT_BUFFERFLAGS_SILENT);
    }
    assert(GetState().silenced == 1000);
    assert(GetState().calls == 4000);
}
