#include <stdio.h>
#include <string.h>

#include "greet.h"
#include "greet_version.h"

int main(int argc, char **argv) {
    char buf[64];
    const char *which = argc > 1 ? argv[1] : "hello";
    if (strcmp(which, "version") == 0) {
        return strcmp(GREET_VERSION, "0.1.0") != 0;
    }
    if (strcmp(which, "hello") == 0) {
        greet("blaze", buf, sizeof buf);
        return strcmp(buf, "Hello, blaze!") != 0;
    }
    greet("", buf, sizeof buf);
    return strcmp(buf, "Hello, world!") != 0;
}
