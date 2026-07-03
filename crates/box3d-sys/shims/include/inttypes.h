#pragma once
#include <stdint.h>

// wasm32: int64_t == long long
#define PRId64 "lld"
#define PRIu64 "llu"
#define PRIx64 "llx"
#define PRId32 "d"
#define PRIu32 "u"
#define PRIx32 "x"
