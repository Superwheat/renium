#pragma once

// Keep a scheduling gap without waiting for Studio's frame jobs, changing the
// system timer resolution, or occupying a worker with a spin wait.
struct BatchPacer
{
    HANDLE timer = nullptr;
    explicit BatchPacer(bool enabled)
    {
        if (enabled) timer = CreateWaitableTimerExW(nullptr, nullptr,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_MODIFY_STATE | SYNCHRONIZE);
    }
    ~BatchPacer() { if (timer) CloseHandle(timer); }
    void Wait(DWORD milliseconds)
    {
        LARGE_INTEGER due{};
        due.QuadPart = -static_cast<LONGLONG>(milliseconds) * 10000;
        if (timer && SetWaitableTimer(timer, &due, 0, nullptr, nullptr, FALSE)
            && WaitForSingleObject(timer, milliseconds + 100) == WAIT_OBJECT_0) return;
        // Older Windows versions can lack high-resolution waitable timers.
        Sleep(milliseconds);
    }
};
