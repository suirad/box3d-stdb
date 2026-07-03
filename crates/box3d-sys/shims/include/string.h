#pragma once
#include <stddef.h>

// mem* resolve from Rust's compiler_builtins at link; str* live in shim.c.
void* memcpy(void* dst, const void* src, size_t n);
void* memmove(void* dst, const void* src, size_t n);
void* memset(void* dst, int c, size_t n);
int memcmp(const void* a, const void* b, size_t n);
char* strncpy(char* dst, const char* src, size_t n);
size_t strlen(const char* s);
int strcmp(const char* a, const char* b);
