#pragma once
#include <cstdint>
#include <cstddef>
extern "C" {
struct PcDecodeLimits {
    uint64_t max_encoded_bytes, max_intermediate_pixels, max_allocation_bytes;
};
struct PcWhitePoint { double x, y; }; // {0,0} preserves the as-shot path.
struct PcImage {
    float *pixels;
    unsigned char *icc;
    size_t icc_size;
    uint32_t width, height, bits, orientation, primaries, transfer, flags;
    uint64_t calibration_applied_pixels, calibration_bypassed_pixels;
    char make[128], model[128], profile[128], error[256];
};
void pc_free(PcImage *out);
}
int fail(PcImage *out, const char *message);
bool allocate(PcImage *out, const PcDecodeLimits *limits);
