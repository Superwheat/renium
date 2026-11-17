#define NOMINMAX
#include <Windows.h>
#include <cassert>
#include <initializer_list>
#include "../renium_batch_pacer.h"

int main()
{
    // There is no Studio, message pump or frame producer in this process.
    // Exercise both the high-resolution timer and its unavailable fallback.
    for (bool enabled : {false, true}) {
        BatchPacer pacer(enabled);
        if (!enabled) assert(!pacer.timer);
        LARGE_INTEGER begin{}, end{}, frequency{};
        QueryPerformanceFrequency(&frequency);
        QueryPerformanceCounter(&begin);
        for (int batch = 0; batch < 4; ++batch) pacer.Wait(1);
        QueryPerformanceCounter(&end);
        assert(end.QuadPart - begin.QuadPart >= frequency.QuadPart / 500);
    }
}
