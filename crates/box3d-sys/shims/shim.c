// libc pieces box3d needs on wasm32-unknown-unknown that aren't provided by
// Rust (compiler_builtins covers mem*; src/stdb.rs covers alloc + libm).
#include <stddef.h>
#include <stdarg.h>

// --- string ---

char* strncpy(char* dst, const char* src, size_t n)
{
	size_t i = 0;
	for (; i < n && src[i]; ++i)
		dst[i] = src[i];
	for (; i < n; ++i)
		dst[i] = 0;
	return dst;
}

size_t strlen(const char* s)
{
	const char* p = s;
	while (*p)
		++p;
	return (size_t)(p - s);
}

int strcmp(const char* a, const char* b)
{
	while (*a && *a == *b)
		++a, ++b;
	return (unsigned char)*a - (unsigned char)*b;
}

// --- stdlib ---

// ponytail: shellsort, O(n^1.3)-ish and deterministic; swap for a real
// introsort if a profile ever blames it.
void qsort(void* base, size_t nmemb, size_t size, int (*cmp)(const void*, const void*))
{
	char* a = base;
	for (size_t gap = nmemb / 2; gap > 0; gap /= 2)
	{
		for (size_t i = gap; i < nmemb; ++i)
		{
			for (size_t j = i; j >= gap && cmp(a + (j - gap) * size, a + j * size) > 0; j -= gap)
			{
				char* x = a + (j - gap) * size;
				char* y = a + j * size;
				for (size_t k = 0; k < size; ++k)
				{
					char t = x[k];
					x[k] = y[k];
					y[k] = t;
				}
			}
		}
	}
}

int abs(int x)
{
	return x < 0 ? -x : x;
}

_Noreturn void exit(int status)
{
	(void)status;
	__builtin_trap();
}

_Noreturn void abort(void)
{
	__builtin_trap();
}

// --- stdio stubs ---
// Only reachable from dump/assert paths; a SpacetimeDB module has no stdout.
// Formatting is intentionally not implemented.

typedef struct FILE FILE;
FILE* stdout = 0;
FILE* stderr = 0;

int printf(const char* fmt, ...)
{
	(void)fmt;
	return 0;
}

int fprintf(FILE* f, const char* fmt, ...)
{
	(void)f;
	(void)fmt;
	return 0;
}

int vfprintf(FILE* f, const char* fmt, va_list ap)
{
	(void)f;
	(void)fmt;
	(void)ap;
	return 0;
}

int vsnprintf(char* buf, size_t n, const char* fmt, va_list ap)
{
	(void)fmt;
	(void)ap;
	if (buf && n > 0)
		buf[0] = 0;
	return 0;
}

int snprintf(char* buf, size_t n, const char* fmt, ...)
{
	(void)fmt;
	if (buf && n > 0)
		buf[0] = 0;
	return 0;
}

int puts(const char* s)
{
	(void)s;
	return 0;
}

FILE* fopen(const char* path, const char* mode)
{
	(void)path;
	(void)mode;
	return 0;
}

int fclose(FILE* f)
{
	(void)f;
	return -1;
}

size_t fwrite(const void* p, size_t sz, size_t n, FILE* f)
{
	(void)p;
	(void)sz;
	(void)n;
	(void)f;
	return 0;
}

size_t fread(void* p, size_t sz, size_t n, FILE* f)
{
	(void)p;
	(void)sz;
	(void)n;
	(void)f;
	return 0;
}

int fscanf(FILE* f, const char* fmt, ...)
{
	(void)f;
	(void)fmt;
	return -1;
}

int fseek(FILE* f, long off, int whence)
{
	(void)f;
	(void)off;
	(void)whence;
	return -1;
}

long ftell(FILE* f)
{
	(void)f;
	return -1;
}
