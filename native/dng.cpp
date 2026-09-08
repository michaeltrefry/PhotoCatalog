#include "decode.h"
#include "dng_host.h"
#include "dng_info.h"
#include "dng_ifd.h"
#include "dng_stream.h"
#include "dng_negative.h"
#include "dng_camera_profile.h"
#include "dng_color_spec.h"
#include "dng_color_space.h"
#include "dng_pixel_buffer.h"
#include "dng_image.h"
#include "dng_tag_types.h"
#include "dng_exceptions.h"
#include "dng_xy_coord.h"
#include <vector>
#include <memory>
#include <cmath>
#include <cstdio>

class BoundedAllocator : public dng_memory_allocator {
public:
    dng_memory_block *Allocate(uint32 size) override {
        if (size > 768u*1024u*1024u) ThrowMemoryFull("PhotoCatalog DNG allocation limit");
        return dng_memory_allocator::Allocate(size);
    }
};
static dng_pixel_buffer row_buffer(const dng_rect &area, uint32 planes, float *data) {
    dng_pixel_buffer buffer;
    buffer.fArea=area; buffer.fPlane=0; buffer.fPlanes=planes;
    buffer.fRowStep=area.W()*planes; buffer.fColStep=planes; buffer.fPlaneStep=1;
    buffer.fPixelType=ttFloat; buffer.fPixelSize=4; buffer.fData=data; buffer.fDirty=true;
    return buffer;
}
extern "C" int pc_dng(const unsigned char *bytes, size_t len, PcImage *out) {
    try {
        BoundedAllocator allocator;
        dng_host host(&allocator); host.SetNeedsMeta(true); host.SetForPreview(false);
        // Prefer the full source RAW over optional enhanced/proxy renderings.
        host.SetIgnoreEnhanced(true);
        dng_stream stream(bytes,len);
        dng_info info; info.Parse(host,stream); info.PostParse(host);
        if (!info.IsValidDNG() || info.fMainIndex<0) return fail(out,"corrupt DNG structure");
        auto *ifd=info.fIFD.at(info.fMainIndex);
        if (!ifd->fImageWidth || !ifd->fImageLength || uint64_t(ifd->fImageWidth)*ifd->fImageLength>100000000)
            return fail(out,"resource limit: DNG dimensions");
        AutoPtr<dng_negative> negative(host.Make_dng_negative());
        negative->Parse(host,stream,info); negative->PostParse(host,stream,info);
        negative->ReadStage1Image(host,stream,info);
        if (info.fMaskIndex!=-1) negative->ReadTransparencyMask(host,stream,info);
        negative->BuildStage2Image(host); negative->BuildStage3Image(host);
        const auto *image=negative->Stage3Image();
        if (!image || image->Planes()!=3) return fail(out,"unsupported DNG camera channel count");
        auto bounds=image->Bounds();
        // Stage 3 coordinates already account for the active area and required opcodes.
        auto crop=negative->DefaultCropArea();
        if (crop.IsEmpty()) crop=bounds;
        if (crop.t<bounds.t||crop.l<bounds.l||crop.b>bounds.b||crop.r>bounds.r)
            return fail(out,"unsupported DNG crop outside image");
        out->width=crop.W(); out->height=crop.H(); out->bits=ifd->fBitsPerSample[0];
        if (ifd->fSampleFormat[0]==3) out->flags|=1;
        if (info.fEnhancedIndex!=-1) out->flags|=2;
        out->orientation=negative->BaseOrientation().GetTIFF(); out->primaries=1; out->transfer=8;
        if (!allocate(out)) return fail(out,"resource limit: DNG output allocation");
        AutoPtr<dng_color_spec> spec(negative->MakeColorSpec(dng_camera_profile_id()));
        dng_camera_profile profile;
        if (negative->GetProfileByID(dng_camera_profile_id(),profile)) {
            snprintf(out->profile,sizeof(out->profile),"%s",profile.Name().Get());
            if (profile.HasHueSatDeltas()) out->flags|=4;
            if (profile.HasLookTable()) out->flags|=8;
        }
        if (negative->HasCameraNeutral()) spec->SetWhiteXY(spec->NeutralToXY(negative->CameraNeutral()));
        else if (negative->HasCameraWhiteXY()) spec->SetWhiteXY(negative->CameraWhiteXY());
        else return fail(out,"unsupported DNG without as-shot white balance");
        const auto matrix=dng_space_sRGB_Linear::Get().MatrixFromPCS()*spec->CameraToPCS();
        const dng_image *mask=negative->TransparencyMask();
        if (mask && !(mask->Bounds()==bounds)) return fail(out,"unsupported DNG mask alignment");
        std::vector<float> row(size_t(out->width)*3), alpha(out->width,1.f);
        const double black=negative->Stage3BlackLevelNormalized();
        for (uint32 y=0;y<out->height;++y) {
            dng_rect area(crop.t+y,crop.l,crop.t+y+1,crop.r);
            auto buffer=row_buffer(area,3,row.data()); image->Get(buffer);
            if (mask) {auto mask_buffer=row_buffer(area,1,alpha.data());mask->Get(mask_buffer);}
            for (uint32 x=0;x<out->width;++x) {
                for (uint32 c=0;c<3;++c) {
                    double value=0; for(uint32 k=0;k<3;++k) value+=matrix[c][k]*(row[3*x+k]-black)/(1.0-black);
                    if (!std::isfinite(value)) return fail(out,"corrupt DNG non-finite sample");
                    out->pixels[(size_t(y)*out->width+x)*4+c]=static_cast<float>(value);
                }
                if (!std::isfinite(alpha[x])||alpha[x]<0||alpha[x]>1) return fail(out,"corrupt DNG transparency mask");
                out->pixels[(size_t(y)*out->width+x)*4+3]=alpha[x];
            }
        }
        return 0;
    } catch (const dng_exception &e) {
        if (e.ErrorCode()==dng_error_memory) return fail(out,"resource limit: DNG SDK allocation failed");
        if (e.ErrorCode()==dng_error_not_yet_implemented || e.ErrorCode()==dng_error_unsupported_dng || e.ErrorCode()==dng_error_host_insufficient)
            return fail(out,"unsupported DNG capability required by source");
        char text[128];snprintf(text,sizeof(text),"DNG SDK error %u: %s",e.ErrorCode(),e.what());return fail(out,text);
    } catch (const std::exception &e) { return fail(out,e.what()); }
    catch (...) {return fail(out,"DNG SDK exception");}
}
