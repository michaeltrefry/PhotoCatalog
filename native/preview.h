#pragma once
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
// Decode/header return categories. Encoder callers continue to treat any nonzero as failure.
typedef enum PcPreviewStatus {
    PC_PREVIEW_OK = 0,
    PC_PREVIEW_ERROR = 1,
    PC_PREVIEW_CORRUPT = 2,
    PC_PREVIEW_RESOURCE_LIMIT = 3
} PcPreviewStatus;
typedef int (*PcPreviewCanceled)(const void *context);
typedef struct PcPreviewBuffer {
    unsigned char *data;
    size_t len;
    uint32_t width, height;
    char error[256];
} PcPreviewBuffer;
int pc_preview_webp(const unsigned char *, uint32_t, uint32_t, int, PcPreviewCanceled, const void *, PcPreviewBuffer *);
int pc_preview_avif(const unsigned char *, uint32_t, uint32_t, int, PcPreviewBuffer *);
int pc_preview_avif_dimensions(const unsigned char *, size_t, PcPreviewBuffer *);
int pc_preview_avif_decode(const unsigned char *, size_t, PcPreviewBuffer *);
void pc_preview_free(PcPreviewBuffer *);
void pc_preview_versions(char *, size_t);
#ifdef __cplusplus
}
#endif
