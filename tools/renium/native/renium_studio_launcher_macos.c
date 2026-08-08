#define _POSIX_C_SOURCE 200809L
#include <limits.h>
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

extern char* realpath(const char*, char*);
extern int setenv(const char*, const char*, int);

int main(int argc, char** argv)
{
    (void)argc;
    char executable[PATH_MAX];
    uint32_t size = sizeof(executable);
    if (_NSGetExecutablePath(executable, &size) != 0)
        return 70;
    char directory[PATH_MAX];
    if (!realpath(executable, directory))
        return 71;
    char* end = strrchr(directory, '/');
    if (!end)
        return 72;
    *end = '\0';
    char studio[PATH_MAX];
    char helper[PATH_MAX];
    const int studio_length =
        snprintf(studio, sizeof(studio), "%s/RobloxStudio.bin", directory);
    if (studio_length < 0 || (size_t)studio_length >= sizeof(studio))
        return 73;
    const int helper_length = snprintf(
            helper,
            sizeof(helper),
            "%s/../Frameworks/ReniumStudioHelper.dylib",
            directory);
    if (helper_length < 0 || (size_t)helper_length >= sizeof(helper))
        return 74;
    if (setenv("DYLD_INSERT_LIBRARIES", helper, 1) != 0)
        return 75;
    argv[0] = studio;
    execv(studio, argv);
    return 76;
}
