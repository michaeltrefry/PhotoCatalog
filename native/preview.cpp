#include "preview.h"
#include <avif/avif.h>
#include <webp/encode.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <exception>

namespace {
constexpr uint64_t max_pixels = 8192ull * 8192;
int fail(PcPreviewBuffer *out, const char *error) {
    std::snprintf(out->error, sizeof(out->error), "%s", error);
    return 1;
}
bool dimensions(uint32_t w, uint32_t h) {
    return w && h && w <= 8192 && h <= 8192 && uint64_t(w)*h <= max_pixels;
}
int copy(PcPreviewBuffer *out, const unsigned char *data, size_t size) {
    if (!size || size > 256u*1024u*1024u) return fail(out,"preview output limit");
    out->data = static_cast<unsigned char *>(std::malloc(size));
    if (!out->data) return fail(out,"preview output allocation");
    std::memcpy(out->data,data,size); out->len=size; return 0;
}
struct WebpContext { PcPreviewCanceled canceled; const void *context; };
int progress(int, const WebPPicture *picture) {
    const auto *state = static_cast<const WebpContext *>(picture->user_data);
    return !state->canceled || !state->canceled(state->context);
}
struct Picture { WebPPicture p{}; ~Picture() { WebPPictureFree(&p); } };
struct Writer { WebPMemoryWriter w{}; ~Writer() { WebPMemoryWriterClear(&w); } };
struct AvifData { avifRWData data=AVIF_DATA_EMPTY; ~AvifData() { avifRWDataFree(&data); } };
}
extern "C" void pc_preview_free(PcPreviewBuffer *out) {
    std::free(out->data); out->data=nullptr; out->len=0;
}
extern "C" void pc_preview_versions(char *out, size_t size) {
    char versions[256]; avifCodecVersions(versions);
    int webp=WebPGetEncoderVersion();
    std::snprintf(out,size,"libwebp=%d.%d.%d;libavif=%s;codecs=%s;aom_encode=%s;aom_decode=%s",
        webp>>16,(webp>>8)&255,webp&255,avifVersion(),versions,
        avifCodecName(AVIF_CODEC_CHOICE_AOM,AVIF_CODEC_FLAG_CAN_ENCODE) ? "available":"unavailable",
        avifCodecName(AVIF_CODEC_CHOICE_AOM,AVIF_CODEC_FLAG_CAN_DECODE) ? "available":"unavailable");
}
extern "C" int pc_preview_webp(const unsigned char *rgb, uint32_t w, uint32_t h, int quality,
    PcPreviewCanceled canceled, const void *context, PcPreviewBuffer *out) {
    try {
        if (!dimensions(w,h) || quality<1 || quality>100) return fail(out,"invalid WebP request");
        WebPConfig config; Picture picture; Writer writer;
        if (!WebPConfigPreset(&config,WEBP_PRESET_DEFAULT,float(quality)) || !WebPPictureInit(&picture.p))
            return fail(out,"WebP ABI mismatch");
        config.method=4; config.thread_level=0; config.lossless=0;
        if (!WebPValidateConfig(&config)) return fail(out,"invalid WebP configuration");
        picture.p.width=int(w); picture.p.height=int(h);
        WebPMemoryWriterInit(&writer.w);
        picture.p.writer=WebPMemoryWrite; picture.p.custom_ptr=&writer.w;
        WebpContext state{canceled,context}; picture.p.progress_hook=progress; picture.p.user_data=&state;
        if (!WebPPictureImportRGB(&picture.p,rgb,int(w)*3)) return fail(out,"WebP RGB import allocation");
        if (!WebPEncode(&config,&picture.p)) return fail(out,picture.p.error_code==VP8_ENC_ERROR_USER_ABORT ? "canceled":"WebP encode failed");
        out->width=w; out->height=h;
        return copy(out,writer.w.mem,writer.w.size);
    } catch (const std::exception &e) { return fail(out,e.what()); }
      catch (...) { return fail(out,"WebP native exception"); }
}
extern "C" int pc_preview_avif(const unsigned char *rgb, uint32_t w, uint32_t h, int quality, PcPreviewBuffer *out) {
    try {
        if (!dimensions(w,h) || quality<1 || quality>100) return fail(out,"invalid AVIF request");
        if (!avifCodecName(AVIF_CODEC_CHOICE_AOM,AVIF_CODEC_FLAG_CAN_ENCODE)) return fail(out,"AOM encoding unavailable");
        std::unique_ptr<avifImage,decltype(&avifImageDestroy)> image(avifImageCreate(w,h,8,AVIF_PIXEL_FORMAT_YUV420),avifImageDestroy);
        std::unique_ptr<avifEncoder,decltype(&avifEncoderDestroy)> encoder(avifEncoderCreate(),avifEncoderDestroy);
        if (!image || !encoder) return fail(out,"AVIF allocation");
        image->yuvRange=AVIF_RANGE_FULL;
        image->colorPrimaries=AVIF_COLOR_PRIMARIES_BT709;
        image->transferCharacteristics=AVIF_TRANSFER_CHARACTERISTICS_SRGB;
        image->matrixCoefficients=AVIF_MATRIX_COEFFICIENTS_BT709;
        avifRGBImage source; avifRGBImageSetDefaults(&source,image.get());
        source.depth=8; source.format=AVIF_RGB_FORMAT_RGB;
        source.pixels=const_cast<unsigned char *>(rgb); source.rowBytes=w*3;
        source.avoidLibYUV=AVIF_TRUE; source.chromaDownsampling=AVIF_CHROMA_DOWNSAMPLING_AVERAGE;
        auto result=avifImageRGBToYUV(image.get(),&source);
        if (result!=AVIF_RESULT_OK) return fail(out,avifResultToString(result));
        encoder->codecChoice=AVIF_CODEC_CHOICE_AOM; encoder->maxThreads=1; encoder->speed=6;
        encoder->quality=quality; encoder->qualityAlpha=100; encoder->autoTiling=AVIF_FALSE;
        encoder->tileRowsLog2=0; encoder->tileColsLog2=0;
        AvifData encoded; result=avifEncoderWrite(encoder.get(),image.get(),&encoded.data);
        if (result!=AVIF_RESULT_OK) return fail(out,avifResultToString(result));
        out->width=w; out->height=h;
        return copy(out,encoded.data.data,encoded.data.size);
    } catch (const std::exception &e) { return fail(out,e.what()); }
      catch (...) { return fail(out,"AVIF native exception"); }
}
extern "C" int pc_preview_avif_dimensions(const unsigned char *encoded, size_t len, PcPreviewBuffer *out) {
    try {
        if (!len || len > 256u*1024u*1024u) return fail(out,"AVIF encoded limit");
        std::unique_ptr<avifDecoder,decltype(&avifDecoderDestroy)> decoder(avifDecoderCreate(),avifDecoderDestroy);
        if (!decoder) return fail(out,"AVIF parser allocation");
        decoder->imageSizeLimit=uint32_t(max_pixels); decoder->imageDimensionLimit=8192; decoder->imageCountLimit=1;
        decoder->ignoreExif=AVIF_TRUE; decoder->ignoreXMP=AVIF_TRUE;
        auto result=avifDecoderSetIOMemory(decoder.get(),encoded,len);
        if (result==AVIF_RESULT_OK) result=avifDecoderParse(decoder.get());
        if (result!=AVIF_RESULT_OK) return fail(out,avifResultToString(result));
        if (!dimensions(decoder->image->width,decoder->image->height)) return fail(out,"AVIF dimensions");
        out->width=decoder->image->width; out->height=decoder->image->height;
        return 0;
    } catch (const std::exception &e) { return fail(out,e.what()); }
      catch (...) { return fail(out,"AVIF header exception"); }
}
extern "C" int pc_preview_avif_decode(const unsigned char *encoded, size_t len, PcPreviewBuffer *out) {
    try {
        if (!avifCodecName(AVIF_CODEC_CHOICE_AOM,AVIF_CODEC_FLAG_CAN_DECODE)) return fail(out,"AOM decoding unavailable");
        std::unique_ptr<avifDecoder,decltype(&avifDecoderDestroy)> decoder(avifDecoderCreate(),avifDecoderDestroy);
        if (!decoder) return fail(out,"AVIF decoder allocation");
        decoder->codecChoice=AVIF_CODEC_CHOICE_AOM; decoder->maxThreads=1;
        decoder->imageSizeLimit=uint32_t(max_pixels); decoder->imageDimensionLimit=8192; decoder->imageCountLimit=1;
        auto result=avifDecoderSetIOMemory(decoder.get(),encoded,len);
        if (result==AVIF_RESULT_OK) result=avifDecoderParse(decoder.get());
        if (result==AVIF_RESULT_OK) result=avifDecoderNextImage(decoder.get());
        if (result!=AVIF_RESULT_OK) return fail(out,avifResultToString(result));
        const auto *image=decoder->image;
        if (!dimensions(image->width,image->height) || image->depth!=8 || image->alphaPlane || image->transformFlags || image->icc.size ||
            image->colorPrimaries!=AVIF_COLOR_PRIMARIES_BT709 || image->transferCharacteristics!=AVIF_TRANSFER_CHARACTERISTICS_SRGB ||
            image->matrixCoefficients!=AVIF_MATRIX_COEFFICIENTS_BT709 || image->yuvRange!=AVIF_RANGE_FULL || image->yuvFormat!=AVIF_PIXEL_FORMAT_YUV420)
            return fail(out,"AVIF cache color/layout contract mismatch");
        out->width=image->width; out->height=image->height; out->len=size_t(out->width)*out->height*3;
        out->data=static_cast<unsigned char *>(std::malloc(out->len));
        if (!out->data) return fail(out,"AVIF RGB allocation");
        avifRGBImage rgb; avifRGBImageSetDefaults(&rgb,image);
        rgb.depth=8; rgb.format=AVIF_RGB_FORMAT_RGB; rgb.avoidLibYUV=AVIF_TRUE;
        rgb.chromaUpsampling=AVIF_CHROMA_UPSAMPLING_BILINEAR;
        rgb.pixels=out->data; rgb.rowBytes=out->width*3;
        result=avifImageYUVToRGB(image,&rgb);
        if (result!=AVIF_RESULT_OK) return fail(out,avifResultToString(result));
        return 0;
    } catch (const std::exception &e) { return fail(out,e.what()); }
      catch (...) { return fail(out,"AVIF decode exception"); }
}
