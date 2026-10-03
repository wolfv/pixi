#ifndef GREET_H
#define GREET_H

#include <stddef.h>

/* Writes "Hello, <name>!" into buf; returns the length it needs. */
size_t greet(const char *name, char *buf, size_t len);

#endif
