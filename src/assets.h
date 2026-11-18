// Files built into the layer (assets/): fonts and the UI documents. The
// layer runs inside Steam's runtime container, where host paths like
// /usr/share aren't visible, so it carries what it needs.
#pragma once

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

struct EmbeddedFile {
    const char* path;  // relative to assets/, e.g. "ui/theme.rcss"
    const unsigned char* data;
    size_t size;
};

extern const struct EmbeddedFile embedded_files[];
extern const size_t embedded_file_count;

#ifdef __cplusplus
}
#endif
