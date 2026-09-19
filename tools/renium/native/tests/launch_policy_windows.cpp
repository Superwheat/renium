// Exercise the actual callback without installing hooks, showing windows or
// changing user focus. The NULL case reproduces a recorded Studio takeover.
#include <Windows.h>
#include <CommCtrl.h>
#include <cstdio>

static DWORD observedForegroundPid = 0;
static bool observedUserInput = false;
static ULONGLONG observedTime = 1000;
static ULONGLONG WINAPI fakeTickCount() { return observedTime; }
static int foregroundCalls = 0, focusCalls = 0, attachCalls = 0, showCalls = 0, positionCalls = 0;
static LONG_PTR windowStyle = 0;
static LONG_PTR windowFlags = 0;
static UINT positionFlags = 0;
static HWND positionAfter = HWND_TOP;
static UINT callbackPositionFlags = 0;
static HWND callbackPositionAfter = HWND_TOP;
static LRESULT callbackActivation = -1;
static void nativeWindowChange();
static HANDLE pendingShow = nullptr;
static int showCommand = -1, postedMessages = 0;
static HWND fakeForeground() { return observedForegroundPid ? reinterpret_cast<HWND>(1) : nullptr; }
static DWORD fakeWindowProcess(HWND window, DWORD* pid) { *pid = window ? observedForegroundPid : 0; return 0; }
static BOOL fakeInputSource(INPUT_MESSAGE_SOURCE* source) {
    *source = {};
    if (observedUserInput) { source->deviceType = IMDT_KEYBOARD; source->originId = IMO_HARDWARE; }
    return TRUE;
}
static LRESULT fakeDefault(HWND, UINT, WPARAM, LPARAM) { return 0; }
static BOOL WINAPI fakeSetForeground(HWND) { ++foregroundCalls; return TRUE; }
static HWND WINAPI fakeSetFocus(HWND) { ++focusCalls; return nullptr; }
static HWND WINAPI fakeGetFocus() { return reinterpret_cast<HWND>(2); }
static BOOL WINAPI fakeAttach(DWORD, DWORD, BOOL) { ++attachCalls; return TRUE; }
static LONG_PTR WINAPI fakeGetStyle(HWND, int index) { return index == GWL_STYLE ? windowFlags : windowStyle; }
static LONG_PTR WINAPI fakeSetStyle(HWND, int, LONG_PTR style) { const auto previous = windowStyle; windowStyle = style; return previous; }
static BOOL WINAPI fakeShow(HWND, int command) {
    ++showCalls; showCommand = command;
    nativeWindowChange();
    if (command == SW_HIDE) windowFlags &= ~WS_VISIBLE;
    else windowFlags |= WS_VISIBLE;
    if (command != SW_HIDE && command != SW_SHOWNA && command != SW_SHOWNOACTIVATE && command != SW_SHOWMINNOACTIVE) ++foregroundCalls;
    return TRUE;
}
static BOOL WINAPI fakeSetProp(HWND, LPCWSTR, HANDLE value) { pendingShow = value; return TRUE; }
static HANDLE WINAPI fakeGetProp(HWND, LPCWSTR) { return pendingShow; }
static HANDLE WINAPI fakeRemoveProp(HWND, LPCWSTR) { HANDLE value = pendingShow; pendingShow = nullptr; return value; }
static UINT WINAPI fakeRegisterMessage(LPCWSTR) { return WM_APP + 123; }
static BOOL WINAPI fakePostMessage(HWND, UINT, WPARAM, LPARAM) { ++postedMessages; return TRUE; }
static BOOL WINAPI fakePosition(HWND, HWND after, int, int, int, int, UINT flags) {
    ++positionCalls; positionFlags = flags; positionAfter = after; return TRUE;
}
static WINDOWPLACEMENT appliedPlacement{};
static BOOL WINAPI fakePlacement(HWND, const WINDOWPLACEMENT* placement) {
    if (!placement) return FALSE;
    appliedPlacement = *placement;
    nativeWindowChange();
    return TRUE;
}
static BOOL WINAPI fakeDestroy(HWND) { nativeWindowChange(); return TRUE; }
static int parentCalls = 0;
static HWND appliedParent = nullptr;
static HWND WINAPI fakeParent(HWND, HWND parent) {
    ++parentCalls; appliedParent = parent;
    if (!(windowStyle & WS_EX_NOACTIVATE)) ++foregroundCalls;
    SetLastError(ERROR_SUCCESS);
    return reinterpret_cast<HWND>(5);
}
#define GetForegroundWindow fakeForeground
#define GetTickCount64 fakeTickCount
#define GetWindowThreadProcessId fakeWindowProcess
#define GetCurrentInputMessageSource fakeInputSource
#define DefSubclassProc fakeDefault
#define SetForegroundWindow fakeSetForeground
#define SetFocus fakeSetFocus
#define GetFocus fakeGetFocus
#define AttachThreadInput fakeAttach
#define GetWindowLongPtrW fakeGetStyle
#define SetWindowLongPtrW fakeSetStyle
#define ShowWindow fakeShow
#define SetWindowPos fakePosition
#define SetWindowPlacement fakePlacement
#define DestroyWindow fakeDestroy
#define SetParent fakeParent
#define SetPropW fakeSetProp
#define GetPropW fakeGetProp
#define RemovePropW fakeRemoveProp
#define RegisterWindowMessageW fakeRegisterMessage
#define PostMessageW fakePostMessage
#include "../renium_launch_windows.cpp"
#define REQUIRE(condition) do { if (!(condition)) { std::fprintf(stderr, "FAILED: %s\n", #condition); return 1; } } while (false)

// Windows raises/activates a selected window before GetForegroundWindow has
// necessarily changed. Taskbar and Alt-Tab requests may have no local input
// source. Exercise the same callbacks both inside Qt's ShowWindow and outside
// any Studio API call, as an OS-driven selection would arrive.
static void nativeWindowChange() {
    WINDOWPOS position{};
    position.hwndInsertAfter = HWND_TOP;
    position.flags = SWP_NOMOVE;
    launchWindowProc(nullptr, WM_WINDOWPOSCHANGING, 0, reinterpret_cast<LPARAM>(&position), 1, 0);
    callbackPositionFlags = position.flags;
    callbackPositionAfter = position.hwndInsertAfter;
    CBTACTIVATESTRUCT activation{};
    callbackActivation = ReniumLaunchHook(HCBT_ACTIVATE, 0, reinterpret_cast<LPARAM>(&activation));
}

int main() {
    REQUIRE(ReniumConfigureLaunch(GetCurrentProcessId()));
    CBTACTIVATESTRUCT activation{};
    const auto invoke = [&] { return ReniumLaunchHook(HCBT_ACTIVATE, 0, reinterpret_cast<LPARAM>(&activation)); };
    observedForegroundPid = 0;
    REQUIRE(invoke() == 0);
    WINDOWPOS position{}; position.hwndInsertAfter = HWND_TOP; position.flags = SWP_NOMOVE;
    launchWindowProc(nullptr, WM_WINDOWPOSCHANGING, 0, reinterpret_cast<LPARAM>(&position), 1, 0);
    REQUIRE(!(position.flags & SWP_NOACTIVATE));
    REQUIRE(position.hwndInsertAfter == HWND_TOP);
    observedForegroundPid = GetCurrentProcessId() + 1;
    REQUIRE(invoke() == 0);
    nativeWindowChange();
    REQUIRE(callbackActivation == 0 && callbackPositionAfter == HWND_TOP);
    REQUIRE(!(callbackPositionFlags & SWP_NOACTIVATE));
    observedForegroundPid = GetCurrentProcessId();
    REQUIRE(invoke() == 0);
    observedForegroundPid = 0;
    activation.fMouse = TRUE;
    REQUIRE(invoke() == 0);
    activation.fMouse = FALSE;
    observedUserInput = true;
    REQUIRE(invoke() == 0);
    observedUserInput = false;
    observedForegroundPid = GetCurrentProcessId() + 1;
    // The actual Qt API sequence must not reach user32's focus or input-queue
    // functions at all. A late CBT refusal alone failed this typing invariant.
    REQUIRE(!launchAttachThreadInput(1, 2, TRUE));
    REQUIRE(!launchSetForegroundWindow(reinterpret_cast<HWND>(3)));
    REQUIRE(launchSetFocus(reinterpret_cast<HWND>(3)) == reinterpret_cast<HWND>(2));
    REQUIRE(foregroundCalls == 0 && focusCalls == 0 && attachCalls == 0);
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(3), SW_SHOWMAXIMIZED));
    REQUIRE(showCalls == 1 && foregroundCalls == 0 && windowStyle == 0);
    REQUIRE(showCommand == SW_SHOWNA && pendingShow);
    REQUIRE(callbackActivation == 1 && callbackPositionAfter == HWND_BOTTOM);
    REQUIRE(callbackPositionFlags & SWP_NOACTIVATE);
    // The protection ends with the native call, even before any user click.
    nativeWindowChange();
    REQUIRE(callbackActivation == 0 && callbackPositionAfter == HWND_TOP);
    REQUIRE(!(callbackPositionFlags & SWP_NOACTIVATE));
    observedForegroundPid = 0;
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(3), SW_SHOWMAXIMIZED));
    REQUIRE(callbackActivation == 1 && callbackPositionAfter == HWND_BOTTOM);
    REQUIRE(callbackPositionFlags & SWP_NOACTIVATE);
    observedForegroundPid = GetCurrentProcessId() + 1;
    // The user selects Studio from the taskbar: Windows activates it before
    // the foreground moves. From here Studio's own calls freeze the order
    // instead of sinking the window under the app the user just left.
    REQUIRE(!userSelectedStudio);
    launchWindowProc(reinterpret_cast<HWND>(3), WM_ACTIVATE, WA_ACTIVE, 0, 1, 0);
    REQUIRE(postedMessages == 1 && userSelectedStudio);
    nativeWindowChange();
    REQUIRE(callbackActivation == 0 && callbackPositionAfter == HWND_TOP);
    REQUIRE(!(callbackPositionFlags & SWP_NOZORDER));
    launchWindowProc(reinterpret_cast<HWND>(3), deferredShowMessage(), 0, 0, 1, 0);
    REQUIRE(showCalls == 2 && pendingShow); // The user switched away before delivery.
    observedForegroundPid = GetCurrentProcessId();
    launchWindowProc(reinterpret_cast<HWND>(3), deferredShowMessage(), 0, 0, 1, 0);
    REQUIRE(showCalls == 3 && showCommand == SW_SHOWMAXIMIZED && !pendingShow);
    foregroundCalls = 0;
    observedForegroundPid = GetCurrentProcessId() + 1;
    WINDOWPLACEMENT placement{sizeof(placement)};
    placement.showCmd = SW_RESTORE;
    placement.rcNormalPosition = {10, 20, 500, 600};
    REQUIRE(launchSetWindowPlacement(reinterpret_cast<HWND>(3), &placement));
    REQUIRE(appliedPlacement.showCmd == SW_SHOWNA && placement.showCmd == SW_RESTORE);
    REQUIRE(appliedPlacement.rcNormalPosition.left == 10 && appliedPlacement.rcNormalPosition.bottom == 600);
    REQUIRE(callbackActivation == 1 && (callbackPositionFlags & SWP_NOZORDER));
    REQUIRE(launchDestroyWindow(reinterpret_cast<HWND>(3)));
    REQUIRE(callbackActivation == 1);
    nativeWindowChange();
    REQUIRE(callbackActivation == 0 && callbackPositionAfter == HWND_TOP);
    REQUIRE(launchSetWindowPos(reinterpret_cast<HWND>(3), HWND_TOP, 0, 0, 100, 100, 0));
    REQUIRE(positionCalls == 1 && (positionFlags & SWP_NOACTIVATE) && (positionFlags & SWP_NOZORDER));
    windowFlags = WS_CHILD;
    REQUIRE(launchSetWindowPos(reinterpret_cast<HWND>(3), HWND_TOP, 0, 0, 100, 100, 0));
    REQUIRE(positionCalls == 2 && !(positionFlags & SWP_NOZORDER));
    windowFlags = 0;
    windowFlags = WS_CHILD | WS_VISIBLE;
    const int shownBeforeParent = showCalls;
    REQUIRE(launchSetParent(reinterpret_cast<HWND>(3), nullptr) == reinterpret_cast<HWND>(5));
    REQUIRE(parentCalls == 1 && appliedParent == nullptr && foregroundCalls == 0);
    REQUIRE(showCalls == shownBeforeParent);
    REQUIRE(windowFlags == (WS_CHILD | WS_VISIBLE) && windowStyle == 0 && GetLastError() == ERROR_SUCCESS);
    windowFlags = 0;
    REQUIRE(launchSetParent(reinterpret_cast<HWND>(3), reinterpret_cast<HWND>(4)) == reinterpret_cast<HWND>(5));
    REQUIRE(parentCalls == 2 && appliedParent == reinterpret_cast<HWND>(4));
    REQUIRE(showCalls == shownBeforeParent && foregroundCalls == 0 && windowStyle == 0);
    // Studio hides its foreground main window while a place settles and
    // shows it again; that re-show may activate, once, even though the
    // foreground has meanwhile fallen back to the user's previous app.
    observedForegroundPid = GetCurrentProcessId();
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(3), SW_HIDE));
    observedForegroundPid = GetCurrentProcessId() + 1;
    foregroundCalls = 0;
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(3), SW_SHOW));
    REQUIRE(showCommand == SW_SHOW && foregroundCalls == 1);
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(3), SW_SHOW));
    REQUIRE(showCommand == SW_SHOWNA && foregroundCalls == 1);
    // A window hidden while another app held the foreground stays in the background.
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(3), SW_HIDE));
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(3), SW_SHOW));
    REQUIRE(showCommand == SW_SHOWNA && foregroundCalls == 1);
    // Destroying the foreground window lets the replacement window activate once.
    observedForegroundPid = GetCurrentProcessId();
    REQUIRE(launchDestroyWindow(reinterpret_cast<HWND>(3)));
    observedForegroundPid = GetCurrentProcessId() + 1;
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(7), SW_SHOW));
    REQUIRE(showCommand == SW_SHOW && foregroundCalls == 2);
    REQUIRE(launchShowWindow(reinterpret_cast<HWND>(7), SW_SHOW));
    REQUIRE(showCommand == SW_SHOWNA && foregroundCalls == 2);
    foregroundCalls = 0;
    // A synthetic loaded PE exercises the same import-table rebinding as Qt,
    // without loading Studio or modifying any system DLL's executable code.
    auto* image = static_cast<unsigned char*>(VirtualAlloc(nullptr, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE));
    REQUIRE(image);
    auto* dos = reinterpret_cast<IMAGE_DOS_HEADER*>(image);
    dos->e_magic = IMAGE_DOS_SIGNATURE; dos->e_lfanew = 0x80;
    auto* nt = reinterpret_cast<IMAGE_NT_HEADERS*>(image + 0x80);
    nt->Signature = IMAGE_NT_SIGNATURE;
    nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress = 0x200;
    auto* imports = reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(image + 0x200);
    imports->Name = 0x280; imports->FirstThunk = 0x300;
    auto* thunk = reinterpret_cast<IMAGE_THUNK_DATA*>(image + 0x300);
    thunk[0].u1.Function = reinterpret_cast<ULONG_PTR>(&SetForegroundWindow);
    thunk[1].u1.Function = reinterpret_cast<ULONG_PTR>(&SetFocus);
    thunk[2].u1.Function = reinterpret_cast<ULONG_PTR>(&AttachThreadInput);
    REQUIRE(bindActivationImports(reinterpret_cast<HMODULE>(image)));
    REQUIRE(bindActivationImports(reinterpret_cast<HMODULE>(image)));
    REQUIRE(!reinterpret_cast<decltype(&SetForegroundWindow)>(thunk[0].u1.Function)(reinterpret_cast<HWND>(3)));
    REQUIRE(reinterpret_cast<decltype(&SetFocus)>(thunk[1].u1.Function)(reinterpret_cast<HWND>(3)) == reinterpret_cast<HWND>(2));
    REQUIRE(!reinterpret_cast<decltype(&AttachThreadInput)>(thunk[2].u1.Function)(1, 2, TRUE));
    REQUIRE(foregroundCalls == 0 && focusCalls == 0 && attachCalls == 0);
    VirtualFree(image, 0, MEM_RELEASE);
    // Loading a place or opening a later dialog can outlive startup. Input
    // protection must not silently expire while another app is focused.
    observedTime += 90001;
    REQUIRE(!launchSetForegroundWindow(reinterpret_cast<HWND>(3)));
    REQUIRE(launchSetFocus(reinterpret_cast<HWND>(3)) == reinterpret_cast<HWND>(2));
    REQUIRE(!launchAttachThreadInput(1, 2, TRUE));
    REQUIRE(invoke() == 0); // External selection remains available indefinitely.
    REQUIRE(foregroundCalls == 0 && focusCalls == 0 && attachCalls == 0);
    // Genuine user activation and cleanup still reach their original APIs.
    observedUserInput = true;
    REQUIRE(launchSetForegroundWindow(reinterpret_cast<HWND>(3)));
    launchSetFocus(reinterpret_cast<HWND>(3));
    REQUIRE(launchAttachThreadInput(1, 2, TRUE));
    REQUIRE(foregroundCalls == 1 && focusCalls == 1 && attachCalls == 1);
    observedUserInput = false;
    REQUIRE(launchAttachThreadInput(1, 2, FALSE));
    REQUIRE(attachCalls == 2);
    puts("PASS: external window selection, scoped background show/raise, activation transitions, keyboard focus, input queues and import binding");
}
