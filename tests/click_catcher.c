/* A window that prints one line per left-button press.
 *
 * Built by tests/own_window.rs with `cc -O2 -lX11`. The X connection dying
 * (the server going away) ends the process instead of aborting on the
 * default I/O error handler.
 */
#include <X11/Xlib.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int io_error(Display *display) {
    (void)display;
    _exit(0);
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: click_catcher x y width height\n");
        return 2;
    }
    int x = atoi(argv[1]);
    int y = atoi(argv[2]);
    int width = atoi(argv[3]);
    int height = atoi(argv[4]);
    if (width <= 0 || height <= 0) {
        fprintf(stderr, "width and height must be positive\n");
        return 2;
    }

    XSetIOErrorHandler(io_error);
    Display *display = XOpenDisplay(NULL);
    if (display == NULL) {
        fprintf(stderr, "no display\n");
        return 1;
    }
    int screen = DefaultScreen(display);
    Window window = XCreateSimpleWindow(
        display,
        RootWindow(display, screen),
        x,
        y,
        (unsigned)width,
        (unsigned)height,
        0,
        BlackPixel(display, screen),
        WhitePixel(display, screen));
    XStoreName(display, window, "click-catcher");
    XSelectInput(display, window, ButtonPressMask | StructureNotifyMask);
    XMapWindow(display, window);
    XFlush(display);

    int ready = 0;
    for (;;) {
        XEvent event;
        XNextEvent(display, &event);
        if (!ready && event.type == MapNotify && event.xmap.window == window) {
            ready = 1;
            printf("ready\n");
            fflush(stdout);
        } else if (event.type == ButtonPress && event.xbutton.button == Button1 &&
                   event.xbutton.window == window) {
            printf("click\n");
            fflush(stdout);
        }
    }
}
