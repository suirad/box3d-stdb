#pragma once
// Build always defines NDEBUG (build.rs), but keep the standard shape.
#ifdef NDEBUG
#define assert(x) ((void)0)
#else
#define assert(x) ((x) ? (void)0 : __builtin_trap())
#endif
