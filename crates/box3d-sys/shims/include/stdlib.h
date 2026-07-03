#pragma once
#include <stddef.h>

// aligned_alloc/free/malloc are real: exported from Rust (src/stdb.rs) over the
// module's global allocator. qsort/exit/abs live in shim.c.
void* aligned_alloc(size_t alignment, size_t size);
void* malloc(size_t size);
void free(void* ptr);
void qsort(void* base, size_t nmemb, size_t size, int (*cmp)(const void*, const void*));
_Noreturn void exit(int status);
_Noreturn void abort(void);
int abs(int x);
