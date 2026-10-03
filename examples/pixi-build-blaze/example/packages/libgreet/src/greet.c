#include "greet.h"

#include <stdio.h>

size_t greet(const char *name, char *buf, size_t len) {
    int n = snprintf(buf, len, "Hello, %s!", (name && *name) ? name : "world");
    return n < 0 ? 0 : (size_t)n;
}
