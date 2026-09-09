#include <libraw/libraw.h>
#include <avif/avif.h>
#include <cstdlib>
#include <cstring>
#include <cstdint>
#include <cstdio>
#include <memory>
#include <exception>
#include <cmath>
#include <algorithm>
#include "decode.h"

// This ABI contains no vendor structs. Compile against the linked library's headers.
extern "C" {
void pc_free(PcImage *out) { free(out->pixels); free(out->icc); out->pixels = nullptr; out->icc = nullptr; }
const char *pc_raw_version() { return libraw_version(); }
const char *pc_avif_version() { return avifVersion(); }
}
int fail(PcImage *out, const char *message) {
    pc_free(out); snprintf(out->error, sizeof(out->error), "%s", message); return -1;
}
bool allocate(PcImage *out, const PcDecodeLimits *limits) {
    uint64_t count = uint64_t(out->width) * out->height;
    if (!count || out->width > 40000 || out->height > 40000 || count > 100000000 || count > limits->max_intermediate_pixels || count*16 > limits->max_allocation_bytes) return false;
    out->pixels = static_cast<float *>(malloc(size_t(count) * 4 * sizeof(float)));
    return out->pixels != nullptr;
}
// Preserve metadata crop before LibRaw rounds CFA origins for demosaicing.
// Crop the developed RGB afterwards, where arbitrary pixel origins are valid.
class OriginalCropRaw : public LibRaw {
public:
    libraw_raw_inset_crop_t vendor_crop{};
    OriginalCropRaw() {
        callbacks.post_identify_cb=[](void *ctx) {
            auto *self=static_cast<OriginalCropRaw *>(static_cast<LibRaw *>(ctx));
            self->vendor_crop=self->imgdata.sizes.raw_inset_crops[0];
            const auto &rect=self->imgdata.makernotes.canon.DefaultCropAbsolute;
            if(self->imgdata.idata.maker_index==LIBRAW_CAMERAMAKER_Canon && rect.l>=0 && rect.t>=0 && rect.r>=rect.l && rect.b>=rect.t) {
                auto &crop=self->vendor_crop;
                if(self->imgdata.sizes.raw_aspect!=LIBRAW_IMAGE_ASPECT_UNKNOWN && crop.cwidth && crop.cheight) {
                    crop.cleft+=rect.l;crop.ctop+=rect.t;
                } else {
                    crop.cleft=rect.l;crop.ctop=rect.t;
                    crop.cwidth=rect.r-rect.l+1;crop.cheight=rect.b-rect.t+1;
                }
            }
        };
    }
};
static int raw_failure(PcImage *out,int status) {
    if(status==LIBRAW_UNSUFFICIENT_MEMORY || status==LIBRAW_TOO_BIG)
        return fail(out,"resource limit: RAW allocation ceiling");
    return fail(out,libraw_strerror(status));
}
extern "C" int pc_raw(const unsigned char *bytes, size_t len, const PcDecodeLimits *limits, PcImage *out) {
    try {
        OriginalCropRaw raw;
        raw.imgdata.rawparams.max_raw_memory_mb = unsigned(std::min<uint64_t>(768, limits->max_allocation_bytes/(1024*1024)));
        if (!raw.imgdata.rawparams.max_raw_memory_mb) return fail(out,"resource limit: RAW allocation allowance below 1 MiB");
        raw.imgdata.rawparams.options &= ~LIBRAW_RAWOPTIONS_CONVERTFLOAT_TO_INT;
        int status = raw.open_buffer(const_cast<unsigned char *>(bytes), len);
        if (status) return raw_failure(out, status);
        out->width = raw.imgdata.sizes.width; out->height = raw.imgdata.sizes.height;
        if (uint64_t(out->width) * out->height > 100000000) return fail(out, "resource limit: RAW exceeds 100 megapixels");
        uint64_t sensor_pixels=uint64_t(raw.imgdata.sizes.raw_width)*raw.imgdata.sizes.raw_height;
        uint64_t developed_pixels=uint64_t(out->width)*out->height;
        if(sensor_pixels>limits->max_intermediate_pixels || developed_pixels>limits->max_intermediate_pixels ||
           sensor_pixels*8>limits->max_allocation_bytes || developed_pixels*16>limits->max_allocation_bytes)
            return fail(out,"resource limit: RAW sensor/development exceeds configured admission");
        snprintf(out->make, sizeof(out->make), "%s", raw.imgdata.idata.make);
        snprintf(out->model, sizeof(out->model), "%s", raw.imgdata.idata.model);
        out->bits = raw.imgdata.color.raw_bps;
        int flip = raw.imgdata.sizes.flip;
        const uint32_t orientations[] = {1,2,4,3,5,8,6,7};
        out->orientation = (flip >= 0 && flip < 8) ? orientations[flip] : 1;
        // Stable full sensor development, not an embedded JPEG. Orientation is applied in Rust once.
        auto &p = raw.imgdata.params;
        float camera_to_rgb[3][3];
        for (int c=0;c<3;++c) {
            if (!(raw.imgdata.color.cam_mul[c]>0) || !std::isfinite(raw.imgdata.color.cam_mul[c]))
                return fail(out,"unsupported RAW missing as-shot white balance");
            for(int k=0;k<3;++k) camera_to_rgb[c][k]=raw.imgdata.color.rgb_cam[c][k];
        }
        // WB belongs before demosaic and highlight handling. With highlight=2,
        // LibRaw scales by the largest WB multiplier to retain sensor headroom,
        // then blends clipped chroma in camera space (dcraw's documented -H 2).
        // Applying WB after clipping instead creates false magenta/green highlights.
        p.half_size = 0; p.user_flip = 0; p.use_camera_wb = 1; p.use_auto_wb = 0;
        p.use_camera_matrix = 1; p.output_color = 0; p.output_bps = 16;
        p.gamm[0] = 1.0; p.gamm[1] = 1.0; p.no_auto_bright = 1;
        p.user_qual = 3; p.highlight = 2;
        status = raw.unpack(); if (status) return raw_failure(out, status);
        if (raw.imgdata.rawdata.float_image || raw.imgdata.rawdata.float3_image || raw.imgdata.rawdata.float4_image)
            return fail(out, "unsupported floating DNG: requires validated ForwardMatrix/profile and transparency-mask rendering; integer conversion is forbidden");
        status = raw.dcraw_process(); if (status) return raw_failure(out, status);
        // scale_colors normalizes the actual WB (including camera white-patch or
        // already-balanced RAW handling) into pre_mul. Undo only its common
        // headroom scale in float; never apply the channel WB twice. The 3-channel
        // output uses merged green, so the fourth CFA multiplier is not a channel.
        float minimum_wb=1.f;
        for(int c=0;c<3;++c) {
            float wb=raw.imgdata.color.pre_mul[c];
            if (!(wb>0) || !std::isfinite(wb))
                return fail(out,"unsupported RAW invalid developed white balance");
            minimum_wb=std::min(minimum_wb,wb);
        }
        const float headroom=1.f/minimum_wb;
        if (!std::isfinite(headroom)) return fail(out,"unsupported RAW white balance range");
        auto *processed = raw.dcraw_make_mem_image(&status);
        if (!processed) return raw_failure(out, status);
        std::unique_ptr<libraw_processed_image_t, decltype(&LibRaw::dcraw_clear_mem)> image(processed, LibRaw::dcraw_clear_mem);
        if (processed->type != LIBRAW_IMAGE_BITMAP || processed->colors != 3 || processed->bits != 16)
            return fail(out, "unsupported RAW development output");
        out->width = processed->width; out->height = processed->height;
        uint32_t crop_left=0,crop_top=0;
        const auto &crop=raw.vendor_crop;
        if(crop.cwidth && crop.cheight && crop.cleft<0xffff && crop.ctop<0xffff) {
            const auto &sizes=raw.imgdata.sizes;
            if(processed->width!=sizes.width || processed->height!=sizes.height)
                return fail(out,"unsupported RAW vendor crop with scaled or diagonal development");
            if(crop.cleft<sizes.left_margin||crop.ctop<sizes.top_margin)
                return fail(out,"unsupported RAW vendor crop outside developed area");
            crop_left=crop.cleft-sizes.left_margin;crop_top=crop.ctop-sizes.top_margin;
            if(uint64_t(crop_left)+crop.cwidth>processed->width || uint64_t(crop_top)+crop.cheight>processed->height)
                return fail(out,"unsupported RAW vendor crop outside developed area");
            out->width=crop.cwidth;out->height=crop.cheight;
        }
        if (!allocate(out, limits)) return fail(out, "resource limit: RAW output allocation");
        const auto *samples = reinterpret_cast<const uint16_t *>(processed->data);
        for (size_t i = 0; i < size_t(out->width) * out->height; ++i) {
            size_t source=(i/out->width+crop_top)*processed->width+(i%out->width+crop_left);
            for (size_t c = 0; c < 3; ++c) {
                float value=0;
                for(size_t k=0;k<3;++k) value+=camera_to_rgb[c][k]*(samples[3*source+k]/65535.0f)*headroom;
                out->pixels[4*i+c]=value;
            }
            out->pixels[4*i+3] = 1;
        }
        out->primaries = 1; out->transfer = 8; // linear sRGB primaries
        return 0;
    } catch (const std::bad_alloc &) { return fail(out,"resource limit: RAW allocation failed"); }
    catch (const std::exception &error) { return fail(out, error.what()); }
    catch (...) { return fail(out, "RAW decoder exception"); }
}
extern "C" int pc_avif(const unsigned char *bytes, size_t len, const PcDecodeLimits *limits, PcImage *out) {
    std::unique_ptr<avifDecoder, decltype(&avifDecoderDestroy)> decoder(avifDecoderCreate(), avifDecoderDestroy);
    if (!decoder) return fail(out, "resource limit: AVIF allocation failed");
    decoder->maxThreads = 1; decoder->imageSizeLimit = 100000000; decoder->imageDimensionLimit = 40000;
    decoder->imageCountLimit = 1;
    auto result = avifDecoderSetIOMemory(decoder.get(), bytes, len);
    if (result == AVIF_RESULT_OK) result = avifDecoderParse(decoder.get());
    if (result == AVIF_RESULT_OK) {
        uint64_t pixels=uint64_t(decoder->image->width)*decoder->image->height;
        if(pixels>limits->max_intermediate_pixels || pixels*16>limits->max_allocation_bytes)
            return fail(out,"resource limit: AVIF exceeds configured admission");
        result = avifDecoderNextImage(decoder.get());
    }
    if (result != AVIF_RESULT_OK) return fail(out, result==AVIF_RESULT_OUT_OF_MEMORY ? "resource limit: AVIF allocation failed" : avifResultToString(result));
    const avifImage *img = decoder->image;
    avifCropRect crop{0,0,img->width,img->height};
    if ((img->transformFlags & AVIF_TRANSFORM_CLAP) && !avifCropRectConvertCleanApertureBox(&crop,&img->clap,img->width,img->height,img->yuvFormat,&decoder->diag))
        return fail(out,"unsupported AVIF fractional or invalid clean aperture");
    if ((img->transformFlags & AVIF_TRANSFORM_PASP) && img->pasp.hSpacing!=img->pasp.vSpacing)
        return fail(out,"unsupported AVIF non-square pixels");
    out->width = crop.width; out->height = crop.height; out->bits = img->depth;
    out->primaries = img->colorPrimaries; out->transfer = img->transferCharacteristics;
    out->orientation = 1;
    unsigned angle = (img->transformFlags & AVIF_TRANSFORM_IROT) ? img->irot.angle : 0;
    // irot describes counter-clockwise quarter turns; EXIF is clockwise.
    const uint32_t rotations[] = {1,8,3,6}; out->orientation = rotations[angle & 3];
    if (img->transformFlags & AVIF_TRANSFORM_IMIR) {
        const uint32_t horizontal[]={2,7,4,5},vertical[]={4,5,2,7};
        out->orientation=(img->imir.axis==1?horizontal:vertical)[angle&3];
    }
    if (!allocate(out, limits)) return fail(out, "resource limit: AVIF output allocation");
    if (img->icc.size) {
        if (img->icc.size > 16*1024*1024) return fail(out, "resource limit: ICC profile");
        out->icc = static_cast<unsigned char *>(malloc(img->icc.size));
        if (!out->icc) return fail(out, "ICC allocation failed");
        memcpy(out->icc, img->icc.data, img->icc.size); out->icc_size = img->icc.size;
    }
    avifRGBImage rgb; avifRGBImageSetDefaults(&rgb, img);
    rgb.depth = 16; rgb.format = AVIF_RGB_FORMAT_RGBA; rgb.alphaPremultiplied = AVIF_FALSE;
    (void)avifRGBImageAllocatePixels(&rgb);
    if (!rgb.pixels) return fail(out, "resource limit: AVIF RGB allocation failed");
    result = avifImageYUVToRGB(img, &rgb);
    if (result == AVIF_RESULT_OK) {
        for (size_t y = 0; y < out->height; ++y) {
            const auto *row = reinterpret_cast<const uint16_t *>(rgb.pixels + (y+crop.y) * rgb.rowBytes)+crop.x*4;
            for (size_t x = 0; x < size_t(out->width)*4; ++x) out->pixels[y*out->width*4+x] = row[x] / 65535.0f;
        }
    }
    avifRGBImageFreePixels(&rgb);
    if (result != AVIF_RESULT_OK) return fail(out, result==AVIF_RESULT_OUT_OF_MEMORY ? "resource limit: AVIF allocation failed" : avifResultToString(result));
    return 0;
}
