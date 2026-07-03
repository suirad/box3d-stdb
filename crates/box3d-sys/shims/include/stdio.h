// Minimal stdio for box3d on wasm32-unknown-unknown. Live code never does file
// I/O (dump/recording paths are inert: shim fopen returns NULL); these exist
// so headers parse and dead paths compile. Stub implementations in shim.c.
#pragma once
#include <stdarg.h>
#include <stddef.h>

typedef struct FILE FILE;
extern FILE* stdout;
extern FILE* stderr;

#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define EOF (-1)

int printf(const char* fmt, ...);
int fprintf(FILE* f, const char* fmt, ...);
int snprintf(char* buf, size_t n, const char* fmt, ...);
int vsnprintf(char* buf, size_t n, const char* fmt, va_list ap);
int vfprintf(FILE* f, const char* fmt, va_list ap);
int puts(const char* s);
FILE* fopen(const char* path, const char* mode);
int fclose(FILE* f);
size_t fwrite(const void* p, size_t sz, size_t n, FILE* f);
size_t fread(void* p, size_t sz, size_t n, FILE* f);
int fseek(FILE* f, long off, int whence);
int fscanf(FILE* f, const char* fmt, ...);
long ftell(FILE* f);
