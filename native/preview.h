#pragma once
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef int (*PcPreviewCanceled)(const void *context);
typedef struct PcPreviewBuffer {
    unsigned char *data;
    size_t len;
    uint32_t width, height;
    char error[256];
} PcPreviewBuffer;
int pc_preview_webp(const unsigned char *, uint32_t, uint32_t, int, PcPreviewCanceled, const void *, PcPreviewBuffer *);
int pc_preview_avif(const unsigned char *, uint32_t, uint32_t, int, PcPreviewBuffer *);
int pc_preview_avif_decode(const unsigned char *, size_t, PcPreviewBuffer *);
void pc_preview_free(PcPreviewBuffer *);
void pc_preview_versions(char *, size_t);
#ifdef __cplusplus
}
#endif
