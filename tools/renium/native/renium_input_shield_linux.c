#define _POSIX_C_SOURCE 200809L
#include <dlfcn.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef struct _XDisplay Display;
typedef struct _XVisual Visual;
typedef struct _XScreen Screen;
typedef struct _XGC *GC;
typedef unsigned long XID;
typedef XID Window;
typedef XID Drawable;
typedef XID Pixmap;
typedef XID Atom;
typedef XID Colormap;
typedef XID Cursor;
typedef int Bool;
typedef int Status;

typedef struct {
    int x;
    int y;
    int width;
    int height;
    int border_width;
    int depth;
    Visual *visual;
    Window root;
    int class;
    int bit_gravity;
    int win_gravity;
    int backing_store;
    unsigned long backing_planes;
    unsigned long backing_pixel;
    Bool save_under;
    Colormap colormap;
    Bool map_installed;
    int map_state;
    long all_event_masks;
    long your_event_mask;
    long do_not_propagate_mask;
    Bool override_redirect;
    Screen *screen;
} XWindowAttributes;

typedef struct {
    Pixmap background_pixmap;
    unsigned long background_pixel;
    Pixmap border_pixmap;
    unsigned long border_pixel;
    int bit_gravity;
    int win_gravity;
    int backing_store;
    unsigned long backing_planes;
    unsigned long backing_pixel;
    Bool save_under;
    long event_mask;
    long do_not_propagate_mask;
    Bool override_redirect;
    Colormap colormap;
    Cursor cursor;
} XSetWindowAttributes;

typedef struct {
    int x;
    int y;
    int width;
    int height;
    int border_width;
    Window sibling;
    int stack_mode;
} XWindowChanges;

typedef struct {
    unsigned long pixel;
    unsigned short red;
    unsigned short green;
    unsigned short blue;
    char flags;
    char pad;
} XColor;

typedef int (*XErrorHandler)(Display *, void *);

typedef struct {
    Display *(*OpenDisplay)(const char *);
    int (*CloseDisplay)(Display *);
    Window (*DefaultRootWindow)(Display *);
    int (*DefaultScreen)(Display *);
    unsigned long (*BlackPixel)(Display *, int);
    Colormap (*DefaultColormap)(Display *, int);
    Status (*AllocColor)(Display *, Colormap, XColor *);
    Atom (*InternAtom)(Display *, const char *, Bool);
    int (*GetWindowProperty)(Display *, Window, Atom, long, long, Bool, Atom, Atom *, int *, unsigned long *, unsigned long *, unsigned char **);
    int (*QueryTree)(Display *, Window, Window *, Window *, Window **, unsigned int *);
    Status (*GetWindowAttributes)(Display *, Window, XWindowAttributes *);
    Bool (*TranslateCoordinates)(Display *, Window, Window, int, int, int *, int *, Window *);
    Window (*CreateWindow)(Display *, Window, int, int, unsigned int, unsigned int, unsigned int, int, unsigned int, Visual *, unsigned long, XSetWindowAttributes *);
    Window (*CreateSimpleWindow)(Display *, Window, int, int, unsigned int, unsigned int, unsigned int, unsigned long, unsigned long);
    int (*ChangeWindowAttributes)(Display *, Window, unsigned long, XSetWindowAttributes *);
    Pixmap (*CreatePixmap)(Display *, Drawable, unsigned int, unsigned int, unsigned int);
    int (*FreePixmap)(Display *, Pixmap);
    GC (*CreateGC)(Display *, Drawable, unsigned long, void *);
    int (*FreeGC)(Display *, GC);
    int (*SetForeground)(Display *, GC, unsigned long);
    int (*FillRectangle)(Display *, Drawable, GC, int, int, unsigned int, unsigned int);
    int (*DrawString)(Display *, Drawable, GC, int, int, const char *, int);
    int (*ClearWindow)(Display *, Window);
    int (*MapWindow)(Display *, Window);
    int (*UnmapWindow)(Display *, Window);
    int (*ConfigureWindow)(Display *, Window, unsigned int, XWindowChanges *);
    int (*DestroyWindow)(Display *, Window);
    int (*GetInputFocus)(Display *, Window *, int *);
    int (*SetInputFocus)(Display *, Window, int, unsigned long);
    int (*Flush)(Display *);
    int (*Free)(void *);
    XErrorHandler (*SetErrorHandler)(XErrorHandler);
    void (*ShapeCombineMask)(Display *, Window, int, int, int, Pixmap, int);
} XApi;

static XApi x;
static void *x11;
static void *xext;
static volatile sig_atomic_t stopping;

enum {
    INPUT_ONLY = 2,
    IS_VIEWABLE = 2,
    REVERT_TO_PARENT = 2,
    CURRENT_TIME = 0,
    SUCCESS = 0,
    XA_CARDINAL = 6,
    ANY_PROPERTY_TYPE = 0,
    CW_OVERRIDE_REDIRECT = 1 << 9,
    CONFIG_X = 1 << 0,
    CONFIG_Y = 1 << 1,
    CONFIG_WIDTH = 1 << 2,
    CONFIG_HEIGHT = 1 << 3,
    CONFIG_SIBLING = 1 << 5,
    CONFIG_STACK = 1 << 6,
    STACK_ABOVE = 0,
    SHAPE_BOUNDING = 0,
    SHAPE_INPUT = 2,
    SHAPE_SET = 0
};

static int ignore_x_error(Display *display, void *event) {
    (void)display;
    (void)event;
    return 0;
}

static bool load_symbol(void *library, const char *name, void *destination) {
    void *symbol = dlsym(library, name);
    if (symbol == NULL) return false;
    memcpy(destination, &symbol, sizeof(symbol));
    return true;
}

#define LOAD_X11(name) if (!load_symbol(x11, "X" #name, &x.name)) return false

static bool load_x11(void) {
    x11 = dlopen("libX11.so.6", RTLD_NOW | RTLD_LOCAL);
    xext = dlopen("libXext.so.6", RTLD_NOW | RTLD_LOCAL);
    if (x11 == NULL || xext == NULL) return false;
    LOAD_X11(OpenDisplay);
    LOAD_X11(CloseDisplay);
    LOAD_X11(DefaultRootWindow);
    LOAD_X11(DefaultScreen);
    LOAD_X11(BlackPixel);
    LOAD_X11(DefaultColormap);
    LOAD_X11(AllocColor);
    LOAD_X11(InternAtom);
    LOAD_X11(GetWindowProperty);
    LOAD_X11(QueryTree);
    LOAD_X11(GetWindowAttributes);
    LOAD_X11(TranslateCoordinates);
    LOAD_X11(CreateWindow);
    LOAD_X11(CreateSimpleWindow);
    LOAD_X11(ChangeWindowAttributes);
    LOAD_X11(CreatePixmap);
    LOAD_X11(FreePixmap);
    LOAD_X11(CreateGC);
    LOAD_X11(FreeGC);
    LOAD_X11(SetForeground);
    LOAD_X11(FillRectangle);
    LOAD_X11(DrawString);
    LOAD_X11(ClearWindow);
    LOAD_X11(MapWindow);
    LOAD_X11(UnmapWindow);
    LOAD_X11(ConfigureWindow);
    LOAD_X11(DestroyWindow);
    LOAD_X11(GetInputFocus);
    LOAD_X11(SetInputFocus);
    LOAD_X11(Flush);
    LOAD_X11(Free);
    LOAD_X11(SetErrorHandler);
    if (!load_symbol(xext, "XShapeCombineMask", &x.ShapeCombineMask)) return false;
    return true;
}

static unsigned long property_value(Display *display, Window window, Atom property, Atom expected) {
    Atom actual_type = 0;
    int actual_format = 0;
    unsigned long count = 0;
    unsigned long remaining = 0;
    unsigned char *data = NULL;
    int status = x.GetWindowProperty(display, window, property, 0, 1, false, expected, &actual_type, &actual_format, &count, &remaining, &data);
    unsigned long value = 0;
    if (status == SUCCESS && data != NULL && count == 1 && actual_format == 32) value = *(unsigned long *)data;
    if (data != NULL) x.Free(data);
    return value;
}

static long window_score(XWindowAttributes *attributes, int viewport_width, int viewport_height) {
    if (attributes->class == INPUT_ONLY || attributes->map_state != IS_VIEWABLE || attributes->width < 320 || attributes->height < 240) return -1;
    if (viewport_width > 0 && viewport_height > 0) {
        long delta = labs((long)attributes->width - viewport_width) + labs((long)attributes->height - viewport_height);
        return 1000000000L - delta;
    }
    return (long)attributes->width * attributes->height;
}

static void score_tree(Display *display, Window window, int viewport_width, int viewport_height, Window *best, long *best_score, int depth) {
    if (depth > 12) return;
    XWindowAttributes attributes;
    if (x.GetWindowAttributes(display, window, &attributes)) {
        long score = window_score(&attributes, viewport_width, viewport_height);
        if (score > *best_score) {
            *best = window;
            *best_score = score;
        }
    }
    Window root = 0;
    Window parent_return = 0;
    Window *children = NULL;
    unsigned int count = 0;
    if (!x.QueryTree(display, window, &root, &parent_return, &children, &count)) return;
    for (unsigned int index = 0; index < count; index++) {
        score_tree(display, children[index], viewport_width, viewport_height, best, best_score, depth + 1);
    }
    if (children != NULL) x.Free(children);
}

static void find_window(Display *display, Window parent, Atom pid_atom, pid_t pid, int viewport_width, int viewport_height, Window *best, Window *owner, long *best_score, int depth) {
    if (depth > 12) return;
    Window root = 0;
    Window parent_return = 0;
    Window *children = NULL;
    unsigned int count = 0;
    if (!x.QueryTree(display, parent, &root, &parent_return, &children, &count)) return;
    for (unsigned int index = 0; index < count; index++) {
        Window child = children[index];
        if ((pid_t)property_value(display, child, pid_atom, XA_CARDINAL) == pid) {
            Window candidate = 0;
            long candidate_score = -1;
            score_tree(display, child, viewport_width, viewport_height, &candidate, &candidate_score, 0);
            if (candidate_score > *best_score) {
                *best = candidate;
                *owner = child;
                *best_score = candidate_score;
            }
        } else {
            find_window(display, child, pid_atom, pid, viewport_width, viewport_height, best, owner, best_score, depth + 1);
        }
    }
    if (children != NULL) x.Free(children);
}

static bool geometry(Display *display, Window window, Window root, int *left, int *top, unsigned int *width, unsigned int *height) {
    XWindowAttributes attributes;
    Window child = 0;
    if (!x.GetWindowAttributes(display, window, &attributes) || attributes.map_state != IS_VIEWABLE) return false;
    if (!x.TranslateCoordinates(display, window, root, 0, 0, left, top, &child)) return false;
    *width = (unsigned int)attributes.width;
    *height = (unsigned int)attributes.height;
    return *width > 0 && *height > 0;
}

static bool belongs_to(Display *display, Window window, Window ancestor) {
    for (int depth = 0; depth < 16 && window != 0; depth++) {
        if (window == ancestor) return true;
        Window root = 0;
        Window parent = 0;
        Window *children = NULL;
        unsigned int count = 0;
        if (!x.QueryTree(display, window, &root, &parent, &children, &count)) return false;
        if (children != NULL) x.Free(children);
        window = parent;
    }
    return false;
}

static Window top_level_window(Display *display, Window window, Window root) {
    Window current = window;
    for (int depth = 0; depth < 16; depth++) {
        Window root_return = 0;
        Window parent = 0;
        Window *children = NULL;
        unsigned int count = 0;
        if (!x.QueryTree(display, current, &root_return, &parent, &children, &count)) break;
        if (children != NULL) x.Free(children);
        if (parent == root) return current;
        if (parent == 0 || parent == current) break;
        current = parent;
    }
    return window;
}

static unsigned long orange_pixel(Display *display) {
    int screen = x.DefaultScreen(display);
    XColor orange = {0};
    orange.red = 245 * 257;
    orange.green = 158 * 257;
    orange.blue = 11 * 257;
    if (x.AllocColor(display, x.DefaultColormap(display, screen), &orange)) return orange.pixel;
    return x.BlackPixel(display, screen);
}

static void shape_visual(Display *display, Window visual, Window root, unsigned int width, unsigned int height, const char *label, unsigned long orange) {
    Pixmap mask = x.CreatePixmap(display, root, width, height, 1);
    GC mask_gc = x.CreateGC(display, mask, 0, NULL);
    x.SetForeground(display, mask_gc, 0);
    x.FillRectangle(display, mask, mask_gc, 0, 0, width, height);
    x.SetForeground(display, mask_gc, 1);
    unsigned int edge = 3;
    x.FillRectangle(display, mask, mask_gc, 0, 0, width, edge);
    x.FillRectangle(display, mask, mask_gc, 0, height - edge, width, edge);
    x.FillRectangle(display, mask, mask_gc, 0, 0, edge, height);
    x.FillRectangle(display, mask, mask_gc, width - edge, 0, edge, height);
    int length = (int)strlen(label);
    int text_x = (int)width - length * 8 - 10;
    if (text_x < 10) text_x = 10;
    x.DrawString(display, mask, mask_gc, text_x, 20, label, length);
    x.ShapeCombineMask(display, visual, SHAPE_BOUNDING, 0, 0, mask, SHAPE_SET);
    Pixmap empty = x.CreatePixmap(display, root, width, height, 1);
    GC empty_gc = x.CreateGC(display, empty, 0, NULL);
    x.SetForeground(display, empty_gc, 0);
    x.FillRectangle(display, empty, empty_gc, 0, 0, width, height);
    x.ShapeCombineMask(display, visual, SHAPE_INPUT, 0, 0, empty, SHAPE_SET);
    GC visual_gc = x.CreateGC(display, visual, 0, NULL);
    x.SetForeground(display, visual_gc, orange);
    x.FillRectangle(display, visual, visual_gc, 0, 0, width, edge);
    x.FillRectangle(display, visual, visual_gc, 0, height - edge, width, edge);
    x.FillRectangle(display, visual, visual_gc, 0, 0, edge, height);
    x.FillRectangle(display, visual, visual_gc, width - edge, 0, edge, height);
    x.DrawString(display, visual, visual_gc, text_x, 20, label, length);
    x.FreeGC(display, visual_gc);
    x.FreeGC(display, empty_gc);
    x.FreePixmap(display, empty);
    x.FreeGC(display, mask_gc);
    x.FreePixmap(display, mask);
}

static void stop_handler(int signal_number) {
    (void)signal_number;
    stopping = 1;
}

int main(int argc, char **argv) {
    if (argc != 6 || !load_x11()) {
        fputs("X11 or XShape is unavailable\n", stderr);
        return 2;
    }
    pid_t target_pid = (pid_t)strtol(argv[1], NULL, 10);
    int viewport_width = (int)strtol(argv[2], NULL, 10);
    int viewport_height = (int)strtol(argv[3], NULL, 10);
    pid_t parent_pid = (pid_t)strtol(argv[4], NULL, 10);
    const char *label = argv[5];
    if (target_pid <= 0 || parent_pid <= 0) return 2;
    signal(SIGTERM, stop_handler);
    signal(SIGINT, stop_handler);
    Display *display = x.OpenDisplay(NULL);
    if (display == NULL) {
        fputs("Could not open the X11 display\n", stderr);
        return 3;
    }
    x.SetErrorHandler(ignore_x_error);
    Window root = x.DefaultRootWindow(display);
    Atom pid_atom = x.InternAtom(display, "_NET_WM_PID", false);
    Atom active_atom = x.InternAtom(display, "_NET_ACTIVE_WINDOW", false);
    Window target = 0;
    Window owner = 0;
    for (int attempt = 0; attempt < 50 && target == 0; attempt++) {
        long score = -1;
        find_window(display, root, pid_atom, target_pid, viewport_width, viewport_height, &target, &owner, &score, 0);
        if (target == 0) {
            struct timespec pause = {0, 16000000};
            nanosleep(&pause, NULL);
        }
    }
    if (target == 0) {
        fputs("Could not find the target X11 window\n", stderr);
        x.CloseDisplay(display);
        return 4;
    }
    owner = top_level_window(display, owner, root);
    XSetWindowAttributes attributes = {0};
    attributes.override_redirect = true;
    int left = 0;
    int top = 0;
    unsigned int width = 1;
    unsigned int height = 1;
    geometry(display, target, root, &left, &top, &width, &height);
    Window blocker = x.CreateWindow(display, root, left, top, width, height, 0, 0, INPUT_ONLY, NULL, CW_OVERRIDE_REDIRECT, &attributes);
    Window visual = x.CreateSimpleWindow(display, root, left, top, width, height, 0, 0, x.BlackPixel(display, x.DefaultScreen(display)));
    x.ChangeWindowAttributes(display, visual, CW_OVERRIDE_REDIRECT, &attributes);
    unsigned long orange = orange_pixel(display);
    shape_visual(display, visual, root, width, height, label, orange);
    bool shown = false;
    Window original_focus = 0;
    puts("ready");
    fflush(stdout);
    struct timespec pause = {0, 16000000};
    while (!stopping && kill(parent_pid, 0) == 0) {
        Window active = property_value(display, root, active_atom, ANY_PROPERTY_TYPE);
        int next_left = 0;
        int next_top = 0;
        unsigned int next_width = 0;
        unsigned int next_height = 0;
        bool visible = (active == owner || belongs_to(display, target, active)) && geometry(display, target, root, &next_left, &next_top, &next_width, &next_height);
        if (!visible) {
            if (shown) {
                x.UnmapWindow(display, visual);
                x.UnmapWindow(display, blocker);
                shown = false;
            }
        } else {
            bool resized = next_width != width || next_height != height;
            width = next_width;
            height = next_height;
            left = next_left;
            top = next_top;
            XWindowChanges changes = {0};
            changes.x = left;
            changes.y = top;
            changes.width = (int)width;
            changes.height = (int)height;
            changes.sibling = owner;
            changes.stack_mode = STACK_ABOVE;
            x.ConfigureWindow(display, blocker, CONFIG_X | CONFIG_Y | CONFIG_WIDTH | CONFIG_HEIGHT | CONFIG_SIBLING | CONFIG_STACK, &changes);
            changes.sibling = blocker;
            x.ConfigureWindow(display, visual, CONFIG_X | CONFIG_Y | CONFIG_WIDTH | CONFIG_HEIGHT | CONFIG_SIBLING | CONFIG_STACK, &changes);
            if (resized) {
                x.ClearWindow(display, visual);
                shape_visual(display, visual, root, width, height, label, orange);
            }
            if (!shown) {
                x.MapWindow(display, blocker);
                x.MapWindow(display, visual);
                shown = true;
            }
            Window focus = 0;
            int revert = 0;
            x.GetInputFocus(display, &focus, &revert);
            if (belongs_to(display, focus, owner)) {
                original_focus = focus;
                x.SetInputFocus(display, visual, REVERT_TO_PARENT, CURRENT_TIME);
            }
        }
        x.Flush(display);
        nanosleep(&pause, NULL);
    }
    Window focus = 0;
    int revert = 0;
    x.GetInputFocus(display, &focus, &revert);
    if (focus == visual && original_focus != 0) x.SetInputFocus(display, original_focus, REVERT_TO_PARENT, CURRENT_TIME);
    x.DestroyWindow(display, visual);
    x.DestroyWindow(display, blocker);
    x.Flush(display);
    x.CloseDisplay(display);
    dlclose(xext);
    dlclose(x11);
    return 0;
}
