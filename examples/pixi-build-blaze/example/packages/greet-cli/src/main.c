#include <stdio.h>

#include "greet.h"

int main(int argc, char **argv) {
    char buf[128];
    greet(argc > 1 ? argv[1] : NULL, buf, sizeof buf);
    printf("%s%s\n", buf, GREET_CLI_SUFFIX);
    return 0;
}
