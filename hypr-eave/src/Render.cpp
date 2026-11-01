// Drawing the glass: two pass elements around each glass surface's own, the backdrop step right
// before it draws and the key right after.

#include "Glass.hpp"
#include "Internals.hpp"

#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/render/gl/GLFramebuffer.hpp>
#include <hyprland/src/render/pass/SurfacePassElement.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>
#include <hyprland/src/desktop/view/LayerSurface.hpp>
#include <hyprland/src/helpers/math/Math.hpp>
#include <hyprland/src/helpers/cm/ColorManagement.hpp>
#include <cmath>

#include "shaders.inc"

using namespace Render;
using namespace Render::GL;

namespace Glass {
    // Per monitor: the backdrop copy and its blurred levels. They grow to the largest asked for
    // and stay, so a popover sliding open doesn't reallocate every frame.
    struct SMonitorGlass {
        SP<IFramebuffer>              backdrop;
        std::vector<SP<IFramebuffer>> levels; // a half, a quarter...
        SP<IFramebuffer>              field, across; // the bend's field, and its blur's halfway
        CRegion                       drawn;  // where the glass was drawn last, monitor px
    };
    static auto& g_monitors = *new std::unordered_map<void*, SMonitorGlass>; // never destroyed: see Protocol.cpp

    // One glass program per kind of surface texture: a dmabuf can come in as an external image,
    // which needs its own sampler type.
    struct SGlassProgram {
        GLuint id = 0;
        GLint  mask, surfSize, fbOrigin, toFb, toSurf, backdrop, bdOrigin, bdScale, bdLimit, wide, wideScale, wideLimit, wideSize, field, fieldScale, fieldLimit, fieldStep, sigma, band,
            fade, thickness, gap, chroma, rim, tint, ink, contrast, margin, count, boxes, fills, chans, bevels, fbSize, rect, reliefs, reliefCount, unit, pictureCount, pictures, pictureShapes;
    };
    static SGlassProgram g_glass[2];
    // The reliefs' data texture: RELIEF_TEXELS float texels a relief, a row each (D-126).
    static GLuint g_reliefs = 0;
    // How much deeper a well goes, pressed all the way, around the finger, logical px.
    constexpr float RELIEF_PRESS = 4.F;

    struct SPassProgram {
        GLuint id = 0;
        GLint  src, srcScale, texel, offset;
        GLint  surfSize, margin, count, boxes, chans; // shape only: which island each pixel is
    };
    static SPassProgram g_down, g_up, g_blur, g_shape[2]; // shape: per kind of surface texture
    static GLuint       g_vao = 0, g_vbo = 0;

    static GLuint       compile(GLenum type, const std::string& src) {
        GLuint      s = glCreateShader(type);
        const char* c = src.c_str();
        glShaderSource(s, 1, &c, nullptr);
        glCompileShader(s);
        GLint ok = 0;
        glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char text[2048];
            glGetShaderInfoLog(s, sizeof(text), nullptr, text);
            log(std::format("shader: {}", text));
            glDeleteShader(s);
            return 0;
        }
        return s;
    }

    GLuint link(const std::string& vert, const std::string& frag) {
        GLuint vs = compile(GL_VERTEX_SHADER, vert), fs = compile(GL_FRAGMENT_SHADER, frag);
        if (!vs || !fs) {
            glDeleteShader(vs);
            glDeleteShader(fs);
            fail("a shader didn't compile");
            return 0;
        }
        GLuint id = glCreateProgram();
        glAttachShader(id, vs);
        glAttachShader(id, fs);
        glLinkProgram(id);
        glDeleteShader(vs);
        glDeleteShader(fs);
        GLint ok = 0;
        glGetProgramiv(id, GL_LINK_STATUS, &ok);
        if (!ok) {
            char text[2048];
            glGetProgramInfoLog(id, sizeof(text), nullptr, text);
            log(std::format("link: {}", text));
            glDeleteProgram(id);
            fail("a shader didn't link");
            return 0;
        }
        return id;
    }

    static std::string replaced(std::string s, const std::string& what, const std::string& with) {
        if (const auto at = s.find(what); at != std::string::npos)
            s.replace(at, what.size(), with);
        return s;
    }

    // A shader that samples the surface's texture, for the kind it is.
    static std::string forTexture(const char* frag, bool external) {
        return replaced(replaced(frag, "EXTENSION", external ? "#extension GL_OES_EGL_image_external_essl3 : require" : ""), "SAMPLER",
                        external ? "samplerExternalOES" : "sampler2D");
    }

    void ensureQuad() {
        if (g_vao)
            return;
        static const float QUAD[] = {0, 0, 1, 0, 0, 1, 1, 1};
        GLint              oldVao = 0, oldVbo = 0;
        glGetIntegerv(GL_VERTEX_ARRAY_BINDING, &oldVao);
        glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &oldVbo);
        glGenVertexArrays(1, &g_vao);
        glGenBuffers(1, &g_vbo);
        glBindVertexArray(g_vao);
        glBindBuffer(GL_ARRAY_BUFFER, g_vbo);
        glBufferData(GL_ARRAY_BUFFER, sizeof(QUAD), QUAD, GL_STATIC_DRAW);
        glEnableVertexAttribArray(0);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, nullptr);
        glBindVertexArray(oldVao);
        glBindBuffer(GL_ARRAY_BUFFER, oldVbo);
    }

    GLuint quadVao() {
        ensureQuad();
        return g_vao;
    }

    static SGlassProgram* ensureGlass(bool external) {
        auto& p = g_glass[external];
        if (p.id)
            return &p;
        p.id = link(SHADER_quad_vert, forTexture(SHADER_glass_frag, external));
        if (!p.id)
            return nullptr;
        auto at     = [&](const char* name) { return glGetUniformLocation(p.id, name); };
        p.mask      = at("mask");
        p.surfSize  = at("surfSize");
        p.fbOrigin  = at("fbOrigin");
        p.toFb      = at("toFb");
        p.toSurf    = at("toSurf");
        p.backdrop  = at("backdrop");
        p.bdOrigin  = at("bdOrigin");
        p.bdScale   = at("bdScale");
        p.bdLimit   = at("bdLimit");
        p.wide      = at("wide");
        p.wideScale = at("wideScale");
        p.wideLimit = at("wideLimit");
        p.wideSize  = at("wideSize");
        p.field      = at("field");
        p.fieldScale = at("fieldScale");
        p.fieldLimit = at("fieldLimit");
        p.fieldStep  = at("fieldStep");
        p.sigma      = at("sigma");
        p.band       = at("band");
        p.fade      = at("fade");
        p.thickness  = at("thickness");
        p.gap       = at("gap");
        p.chroma    = at("chroma");
        p.rim       = at("rim");
        p.tint      = at("tint");
        p.ink       = at("ink");
        p.contrast  = at("contrast");
        p.margin    = at("margin");
        p.count     = at("count");
        p.boxes     = at("boxes");
        p.fills     = at("fills");
        p.chans     = at("chans");
        p.bevels    = at("bevels");
        p.fbSize    = at("fbSize");
        p.rect      = at("rect");
        p.reliefs     = at("reliefs");
        p.reliefCount = at("reliefCount");
        p.unit        = at("unit");
        p.pictureCount  = at("pictureCount");
        p.pictures      = at("pictures");
        p.pictureShapes = at("pictureShapes");
        ensureQuad();
        return &p;
    }

    static bool ensurePass(SPassProgram& p, const std::string& frag) {
        if (p.id)
            return true;
        p.id = link(SHADER_pass_vert, frag);
        if (!p.id)
            return false;
        p.src      = glGetUniformLocation(p.id, "src");
        p.srcScale = glGetUniformLocation(p.id, "srcScale");
        p.texel    = glGetUniformLocation(p.id, "texel");
        p.offset   = glGetUniformLocation(p.id, "offset");
        p.surfSize = glGetUniformLocation(p.id, "surfSize");
        p.margin   = glGetUniformLocation(p.id, "margin");
        p.count    = glGetUniformLocation(p.id, "count");
        p.boxes    = glGetUniformLocation(p.id, "boxes");
        p.chans    = glGetUniformLocation(p.id, "chans");
        ensureQuad();
        return true;
    }

    // Grows `fb` to hold at least `need`, in the format of `like`, the framebuffer it's blitted
    // from or drawn back into, or in `format`. Only a buffer in `like`'s format failing turns the
    // glass off: another format can fall back to it.
    bool ensureFB(SP<IFramebuffer>& fb, const SP<IFramebuffer>& like, const Vector2D& need, DRMFormat format = 0) {
        if (!format)
            format = like->m_drmFormat;
        if (!fb)
            fb = g_pHyprRenderer->createFB("hypr-eave");
        if (!fb->isAllocated() || fb->m_size.x < need.x || fb->m_size.y < need.y || fb->m_drmFormat != format) {
            const Vector2D size = {std::max(need.x, fb->isAllocated() ? fb->m_size.x : 0.0), std::max(need.y, fb->isAllocated() ? fb->m_size.y : 0.0)};
            fb->release();
            if (!fb->alloc(size.x, size.y, format)) {
                if (format == like->m_drmFormat)
                    fail("couldn't allocate a buffer");
                return false;
            }
        }
        fb->setImageDescription(like->imageDescription());
        return true;
    }

    // GL state we touch, put back as Hyprland left it. Viewports go through Hyprland's cache
    // (setViewport), and the target's bind() restores its own.
    struct SGLState {
        GLint prog = 0, vao = 0, active = 0, tex[5] = {}, read = 0, draw = 0, pack = 0, unpack = 0, alignment = 4, rowLength = 0, skipRows = 0, skipPixels = 0;
        bool  scissor = false;
        void  save() {
            glGetIntegerv(GL_CURRENT_PROGRAM, &prog);
            glGetIntegerv(GL_VERTEX_ARRAY_BINDING, &vao);
            glGetIntegerv(GL_ACTIVE_TEXTURE, &active);
            glGetIntegerv(GL_PIXEL_UNPACK_BUFFER_BINDING, &unpack);
            glGetIntegerv(GL_UNPACK_ALIGNMENT, &alignment);
            glGetIntegerv(GL_UNPACK_ROW_LENGTH, &rowLength);
            glGetIntegerv(GL_UNPACK_SKIP_ROWS, &skipRows);
            glGetIntegerv(GL_UNPACK_SKIP_PIXELS, &skipPixels);
            for (int i = 0; i < 5; i++) {
                glActiveTexture(GL_TEXTURE0 + i);
                glGetIntegerv(GL_TEXTURE_BINDING_2D, &tex[i]);
            }
            glActiveTexture(GL_TEXTURE0);
            glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read);
            glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw);
            glGetIntegerv(GL_PIXEL_PACK_BUFFER_BINDING, &pack);
            scissor = glIsEnabled(GL_SCISSOR_TEST);
            g_pHyprOpenGL->setCapStatus(GL_SCISSOR_TEST, false);
        }
        void restore() {
            glBindVertexArray(vao);
            glUseProgram(prog);
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, unpack);
            glPixelStorei(GL_UNPACK_ALIGNMENT, alignment);
            glPixelStorei(GL_UNPACK_ROW_LENGTH, rowLength);
            glPixelStorei(GL_UNPACK_SKIP_ROWS, skipRows);
            glPixelStorei(GL_UNPACK_SKIP_PIXELS, skipPixels);
            for (int i = 0; i < 5; i++) {
                glActiveTexture(GL_TEXTURE0 + i);
                glBindTexture(GL_TEXTURE_2D, tex[i]);
            }
            glActiveTexture(active);
            glBindFramebuffer(GL_READ_FRAMEBUFFER, read);
            glBindFramebuffer(GL_DRAW_FRAMEBUFFER, draw);
            glBindBuffer(GL_PIXEL_PACK_BUFFER, pack);
            g_pHyprOpenGL->blend(true); // Hyprland's premultiplied blending, as everything draws
            g_pHyprOpenGL->setCapStatus(GL_SCISSOR_TEST, scissor);
        }
    };

    static bool glFine(const char* where) {
#ifdef HYPR_EAVE_DEBUG
        if (const GLenum e = glGetError(); e != GL_NO_ERROR) {
            fail(std::format("GL error {:#x} in {}", e, where));
            return false;
        }
#else
        (void)where;
#endif
        return true;
    }

    // How far past the shape eave reports an island's pixels may reach while it wobbles, logical px.
    constexpr float ISLAND_MARGIN = 12.F;

    // How far around a surface the glass reads: the bend, the frost's reach, and a margin.
    static float reach(const SLook& look, float scale) {
        return ((look.thickness + look.gap) * (1.F + look.chroma) + look.frost * (1 << look.depth) + 2.F) * scale;
    }

    // Monitor px to framebuffer px, for a point.
    static Vector2D toFbPoint(const PHLMONITOR& mon, const Vector2D& p) {
        CBox b{p, {0, 0}};
        b.transform(Math::wlTransformToHyprutils(Math::invertTransform(mon->m_transform)), mon->m_transformedSize.x, mon->m_transformedSize.y);
        return b.pos();
    }

    static CBox toFbBox(const PHLMONITOR& mon, CBox box) {
        return box.transform(Math::wlTransformToHyprutils(Math::invertTransform(mon->m_transform)), mon->m_transformedSize.x, mon->m_transformedSize.y);
    }

    // The ink's contrast math assumes 8-bit-ish sRGB: off for HDR and wide gamut.
    static bool sdr(const SP<IFramebuffer>& fb) {
        const auto desc = fb ? fb->imageDescription() : NColorManagement::PImageDescription{};
        if (!desc)
            return true;
        const auto& v = desc->value();
        return (v.transferFunction == NColorManagement::CM_TRANSFER_FUNCTION_GAMMA22 || v.transferFunction == NColorManagement::CM_TRANSFER_FUNCTION_SRGB) &&
            (!v.primariesNameSet || v.primariesNamed == NColorManagement::CM_PRIMARIES_SRGB);
    }

    static bool hasGlass(const SState& state) {
        return std::ranges::any_of(state.regions, [](const SRegion& r) { return r.kind != EAVE_GLASS_SURFACE_V1_KIND_SOLID; });
    }

    // The glass reads around every pixel it draws, so a change anywhere it can see redraws all of
    // it. Added to this frame's record in the damage ring too, so older buffers catch up. Where the
    // glass was last drawn counts too: a surface that just shrank leaves it there otherwise.
    void onRenderBegin(CRenderContext& ctx) {
        if (!enabled())
            return;
        auto& rd  = ctx.m_data;
        auto  mon = rd.pMonitor.lock();
        if (!mon)
            return;

        CRegion extra;
        float   most = 0.F;
        for (auto* glass : allGlass()) {
            if (!hasGlass(glass->m_current))
                continue;
            const auto surface = glass->m_surface.lock();
            const auto hl      = surface ? Desktop::View::CWLSurface::fromResource(surface) : nullptr;
            const auto global  = hl ? hl->getSurfaceBoxGlobal() : std::nullopt;
            if (!global)
                continue;
            const auto box  = CBox{global->pos() - mon->m_position, global->size()}.scale(mon->m_scale).round();
            const auto far_ = reach(glass->m_current.look, mon->m_scale);
            most            = std::max(most, far_);
            if (!rd.damage.copy().intersect(box.copy().expand(far_)).empty())
                extra.add(box);
        }
        auto& drawn = g_monitors[mon.get()].drawn;
        if (!extra.empty() || !rd.damage.copy().intersect(drawn.copy().expand(most)).empty())
            extra.add(drawn);
        if (extra.empty())
            return;

        rd.damage.add(extra);
        rd.finalDamage.add(extra);
        Internals::thisFrameDamage(mon->m_damage).add(extra);
    }

    // The field's format: half floats, so its slope is smooth even a band in. Some GPUs can't
    // draw into them, and then it's the framebuffer's, 8 bits.
    static bool g_halfFloat = true;

    static bool ensureField(SP<IFramebuffer>& fb, const SP<IFramebuffer>& like, const Vector2D& need) {
        if (g_halfFloat) {
            if (ensureFB(fb, like, need, DRM_FORMAT_ABGR16161616F)) {
                glBindFramebuffer(GL_DRAW_FRAMEBUFFER, GLFB(fb)->getFBID());
                if (glCheckFramebufferStatus(GL_DRAW_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE)
                    return true;
            }
            g_halfFloat = false;
            log("no half-float buffers: the bend's field is 8-bit");
        }
        return ensureFB(fb, like, need);
    }

    // The bend's field's gaussian for a look, s px: two fifths of the band.
    static float fieldSigma(const SLook& look, float scale) {
        return std::max(look.band * scale / 2.5F, 0.5F);
    }

    // The inverse error function, as glass.frag's (Giles, 2010), for |x| up to 0.995.
    static float erfinv(float x) {
        const float w = -std::log((1.F - x) * (1.F + x)) - 2.5F;
        float       p = 2.81022636e-08F;
        for (float c : {3.43273939e-07F, -3.5233877e-06F, -4.39150654e-06F, 0.00021858087F, -0.00125372503F, -0.00417768164F, 0.246640727F, 1.50140941F})
            p = c + p * w;
        return p * x;
    }

    // An island's bevel, s px: the band, or on an island too narrow for two, as deep as the
    // field lets glass.frag see into it. That's where the field across a strip `w` px from each
    // edge to its middle peaks, read as a straight edge would be, so the bevel ends flat exactly
    // there and the two sides meet in one smooth round: two bevels still sloping met in a crease.
    static float bevelOf(const float* box, float band, float sigma) {
        const float w     = 0.5F * std::min(box[2] - box[0], box[3] - box[1]);
        const float r     = sigma * std::sqrt(2.F);
        const float peak  = std::clamp(2.F * std::erf(std::max(w, 0.F) / r) - 1.F, -0.995F, 0.995F);
        return std::clamp(r * erfinv(peak), 1.F, std::max(band, 1.F));
    }

    // What eave said, in s px, for the shaders. Each glass island also gets one of the field's four
    // channels, so its bend is its own shape's: an island whose field reaches another's (3 sigmas,
    // and the margin either may wobble past its box) takes a channel the other hasn't. Islands
    // that overlap are still two shapes, and are kept apart the same way. With more neighbours
    // than channels, the one whose nearest user is farthest is shared. And each gets its bevel.
    static void records(SJob& job, float scale) {
        const float k   = job.scale;
        const float far = 3.F * fieldSigma(job.state.look, scale) + 2.F * ISLAND_MARGIN * k;
        job.count       = 0;
        for (auto const& r : job.state.regions) {
            if (job.count >= sc<int>(MAX_REGIONS))
                break;
            const int i         = job.count++;
            float*    b         = &job.boxes[i * 4];
            float*    f         = &job.fills[i * 4];
            b[0]                = r.x * k;
            b[1]                = r.y * k;
            b[2]                = (r.x + r.w) * k;
            b[3]                = (r.y + r.h) * k;
            f[0]                = ((r.fill >> 16) & 0xFF) / 255.F;
            f[1]                = ((r.fill >> 8) & 0xFF) / 255.F;
            f[2]                = (r.fill & 0xFF) / 255.F;
            // w: -1 for solid content, else which way the island's ink goes, 0 to 1.
            const bool solid    = r.kind == EAVE_GLASS_SURFACE_V1_KIND_SOLID;
            f[3]                = solid ? -1.F : r.ink / 255.F;
            job.chans[i]        = solid ? -1 : 0;
            job.bevels[i]       = solid ? 0.F : bevelOf(b, job.state.look.band * scale, fieldSigma(job.state.look, scale));
            if (solid)
                continue;
            // How near each channel's nearest island is.
            float nearest[4] = {INFINITY, INFINITY, INFINITY, INFINITY};
            for (int j = 0; j < i; j++) {
                if (job.chans[j] < 0)
                    continue;
                const float* o  = &job.boxes[j * 4];
                const float  dx = std::max({o[0] - b[2], b[0] - o[2], 0.F});
                const float  dy = std::max({o[1] - b[3], b[1] - o[3], 0.F});
                const float  d  = std::hypot(dx, dy);
                nearest[job.chans[j]] = std::min(nearest[job.chans[j]], d);
            }
            int best = 0;
            for (int c = 0; c < 4; c++) {
                if (nearest[c] >= far) {
                    best = c;
                    break;
                }
                if (nearest[c] > nearest[best])
                    best = c;
            }
            job.chans[i] = best;
        }

        // The reliefs, as the glass reads them: the box (x0, y0, x1, y1); the corner, the kind,
        // the depth (a well's below the glass, negative) and how much deeper the press takes it;
        // the milk, where it ends across, and the light's place; how bright the light is.
        job.reliefCount = 0;
        for (auto const& r : job.state.reliefs) {
            if (job.reliefCount >= sc<int>(MAX_RELIEFS))
                break;
            float* t = &job.reliefData[job.reliefCount++ * RELIEF_TEXELS * 4];
            t[0]     = r.x * k;
            t[1]     = r.y * k;
            t[2]     = (r.x + r.w) * k;
            t[3]     = (r.y + r.h) * k;
            t[4]     = r.radius * k;
            t[5]     = sc<float>(r.kind);
            t[6]     = (r.kind == 0 ? -1.F : 1.F) * r.depth * k;
            t[7]     = r.kind == 0 ? r.press * RELIEF_PRESS * k : 0.F;
            t[8]     = r.milk;
            t[9]     = r.fillX * k;
            t[10]    = r.lightX * k;
            t[11]    = r.lightY * k;
            t[12]    = r.lit;
            t[13] = t[14] = t[15] = 0.F;
        }

        // The pictures (D-129): the box (x0, y0, x1, y1), and the corner and the opacity.
        job.pictureCount = 0;
        for (auto const& p : job.state.pictures) {
            if (job.pictureCount >= sc<int>(MAX_PICTURES))
                break;
            const int i                  = job.pictureCount++;
            job.pictureBoxes[i * 4 + 0]  = p.x * k;
            job.pictureBoxes[i * 4 + 1]  = p.y * k;
            job.pictureBoxes[i * 4 + 2]  = (p.x + p.w) * k;
            job.pictureBoxes[i * 4 + 3]  = (p.y + p.h) * k;
            job.pictureShapes[i * 2 + 0] = p.radius * k;
            job.pictureShapes[i * 2 + 1] = p.opacity;
        }
    }

    // The reliefs' rows, on texture unit 4.
    static void uploadReliefs(const SJob& job) {
        glActiveTexture(GL_TEXTURE4);
        if (!g_reliefs) {
            glGenTextures(1, &g_reliefs);
            glBindTexture(GL_TEXTURE_2D, g_reliefs);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA32F, RELIEF_TEXELS, MAX_RELIEFS, 0, GL_RGBA, GL_FLOAT, nullptr);
        } else
            glBindTexture(GL_TEXTURE_2D, g_reliefs);
        if (job.reliefCount > 0) {
            glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
            glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
            glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);
            glPixelStorei(GL_UNPACK_SKIP_ROWS, 0);
            glPixelStorei(GL_UNPACK_SKIP_PIXELS, 0);
            glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, RELIEF_TEXELS, job.reliefCount, GL_RGBA, GL_FLOAT, job.reliefData.data());
        }
    }

    // The bend's field: the surface's coverage, blurred by a gaussian a band wide. Its value says
    // how deep into the glass a pixel is and its slope which way is out, both smooth from pixel to
    // pixel, which rings of samples around each pixel weren't: they changed in steps as the edge
    // crossed them. The gaussian's sigma is two fifths of the band: it has all but settled a band
    // in, and goes there as the rings did.
    //
    // It's drawn small, at most 4 texels to the sigma: a blur that wide has nothing finer. Each
    // texel averages the coverage under it first, so the edge can't fall between samples.
    //
    // One field per island, in its channel (`records`): each pixel's coverage counts only for the
    // island it belongs to, so an island near another isn't bent by it.
    static bool drawField(SJob& job, SMonitorGlass& fbs, const SP<IFramebuffer>& target, float scale) {
        const bool external = job.tex->m_type == Render::TEXTURE_EXTERNAL;
        if (!ensurePass(g_shape[external], forTexture(SHADER_shape_frag, external)) || !ensurePass(g_blur, SHADER_blur_frag))
            return false;

        const float    sigma = fieldSigma(job.state.look, scale); // s px
        const float    step  = std::max(sigma / 4.F, 1.F);                         // s px per texel
        const Vector2D size  = {std::ceil(job.box.w / step), std::ceil(job.box.h / step)};
        if (!ensureField(fbs.field, target, size) || !ensureField(fbs.across, target, size))
            return false;

        glBindVertexArray(g_vao);
        g_pHyprOpenGL->blend(false);
        glActiveTexture(GL_TEXTURE0);
        g_pHyprOpenGL->setViewport(0, 0, size.x, size.y);

        // 1. The coverage, a texel's worth at a time, into the field.
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, GLFB(fbs.field)->getFBID());
        auto& shape = g_shape[external];
        glUseProgram(shape.id);
        job.tex->bind();
        job.tex->setTexParameter(GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        job.tex->setTexParameter(GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glUniform1i(shape.src, 0);
        glUniform2f(shape.srcScale, size.x * step / job.box.w, size.y * step / job.box.h);
        glUniform2f(shape.texel, step / job.box.w, step / job.box.h);
        glUniform1f(shape.offset, std::clamp(std::ceil(step), 1.F, 4.F));
        glUniform2f(shape.surfSize, job.box.w, job.box.h);
        glUniform1f(shape.margin, ISLAND_MARGIN * scale);
        glUniform1i(shape.count, job.count);
        glUniform4fv(shape.boxes, MAX_REGIONS, job.boxes.data());
        glUniform1iv(shape.chans, MAX_REGIONS, job.chans.data());
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        job.tex->unbind();

        // 2. Blurred across into `across`, and down back into the field.
        glUseProgram(g_blur.id);
        glUniform1i(g_blur.src, 0);
        glUniform1f(g_blur.offset, sigma / step);
        auto blur = [&](const SP<IFramebuffer>& src, const SP<IFramebuffer>& dst, Vector2D dir) {
            glBindFramebuffer(GL_DRAW_FRAMEBUFFER, GLFB(dst)->getFBID());
            glBindTexture(GL_TEXTURE_2D, src->getTexture()->m_texID);
            glUniform2f(g_blur.srcScale, size.x / src->m_size.x, size.y / src->m_size.y);
            glUniform2f(g_blur.texel, dir.x / src->m_size.x, dir.y / src->m_size.y);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        };
        blur(fbs.field, fbs.across, {1, 0});
        blur(fbs.across, fbs.field, {0, 1});

        job.field     = fbs.field;
        job.fieldSize = size;
        job.fieldStep  = step;
        job.fieldSigma = sigma;
        return true;
    }

    // Before the surface: copy what's under and around it, and frost it.
    static void takeBackdrop(CRenderContext& ctx, SJob& job, CGlassSurface* glass) {
        auto& rd  = ctx.m_data;
        auto  mon = job.mon.lock();
        if (!enabled() || !mon || !rd.currentFB || rd.damage.copy().intersect(job.box).empty())
            return;

        const auto target = rd.currentFB;
        job.copy          = job.fbBox.copy().expand(reach(job.state.look, mon->m_scale)).intersection(CBox{{}, target->m_size}).round();
        if (job.copy.empty())
            return;

        auto& fbs = g_monitors[mon.get()];
        if (!ensureFB(fbs.backdrop, target, job.copy.size()))
            return;

        SGLState gl;
        gl.save();

        // 1. Copy the framebuffer around the glass into the corner of our buffer.
        glBindFramebuffer(GL_READ_FRAMEBUFFER, GLFB(target)->getFBID());
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, GLFB(fbs.backdrop)->getFBID());
        const auto& c = job.copy;
        glBlitFramebuffer(c.x, c.y, c.x + c.w, c.y + c.h, 0, 0, c.w, c.h, GL_COLOR_BUFFER_BIT, GL_NEAREST);

        // 2. Frost it: blur down `depth` halvings and back up to a half.
        job.sampled    = fbs.backdrop;
        job.level      = {1, 1};
        job.wide       = fbs.backdrop;
        job.wideLevel  = {1, 1};
        const auto& lk = job.state.look;
        bool        ok = true;
        if (lk.frost > 0 && ensurePass(g_down, SHADER_down_frag) && ensurePass(g_up, SHADER_up_frag)) {
            const int depth = std::clamp(lk.depth, 1, 6);
            fbs.levels.resize(std::max<size_t>(fbs.levels.size(), depth));
            std::vector<Vector2D> sizes;
            for (int i = 0; i < depth && ok; i++) {
                const double div = 2 << i;
                sizes.push_back({std::ceil(c.w / div), std::ceil(c.h / div)});
                ok = ensureFB(fbs.levels[i], target, sizes.back());
            }
            if (ok) {
                glBindVertexArray(g_vao);
                g_pHyprOpenGL->blend(false);
                glActiveTexture(GL_TEXTURE0);
                auto pass = [&](SPassProgram& prog, const SP<IFramebuffer>& src, const Vector2D& srcContent, const SP<IFramebuffer>& dst, const Vector2D& dstContent) {
                    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, GLFB(dst)->getFBID());
                    g_pHyprOpenGL->setViewport(0, 0, dstContent.x, dstContent.y);
                    glUseProgram(prog.id);
                    glBindTexture(GL_TEXTURE_2D, src->getTexture()->m_texID);
                    glUniform1i(prog.src, 0);
                    glUniform2f(prog.srcScale, srcContent.x / src->m_size.x, srcContent.y / src->m_size.y);
                    glUniform2f(prog.texel, 1.0 / src->m_size.x, 1.0 / src->m_size.y);
                    glUniform1f(prog.offset, lk.frost);
                    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
                };
                pass(g_down, fbs.backdrop, c.size(), fbs.levels[0], sizes[0]);
                for (int i = 1; i < depth; i++)
                    pass(g_down, fbs.levels[i - 1], sizes[i - 1], fbs.levels[i], sizes[i]);
                for (int i = depth - 1; i > 0; i--)
                    pass(g_up, fbs.levels[i], sizes[i], fbs.levels[i - 1], sizes[i - 1]);
                job.sampled   = fbs.levels[0];
                job.level     = {sizes[0].x / c.w, sizes[0].y / c.h};
                job.wide      = fbs.levels[depth - 1]; // only blurred down: the way up starts from it
                job.wideLevel = {sizes[depth - 1].x / c.w, sizes[depth - 1].y / c.h};
            }
        }
        if (ok) {
            records(job, mon->m_scale);
            ok = drawField(job, fbs, target, mon->m_scale);
        }
        if (ok && glass && job.state.reportCell)
            reportBackdrop(*glass, job);

        target->bind(); // and its viewport, for the surface that draws next
        gl.restore();
        job.ready = ok && glFine("backdrop");
    }

    // After the surface: key the glass in where its islands are, refracted at their edges.
    static void keyGlass(CRenderContext& ctx, SJob& job) {
        auto mon = job.mon.lock();
        if (!job.ready || !mon || !enabled())
            return;
        const bool external = job.tex->m_type == Render::TEXTURE_EXTERNAL;
        const auto prog     = ensureGlass(external);
        if (!prog)
            return;

        auto&      rd     = ctx.m_data;
        const auto fbSize = rd.currentFB->m_size;
        const auto& lk    = job.state.look;
        const float S     = mon->m_scale;

        SGLState gl;
        gl.save();

        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, job.sampled->getTexture()->m_texID);
        glActiveTexture(GL_TEXTURE2);
        glBindTexture(GL_TEXTURE_2D, job.field->getTexture()->m_texID);
        glActiveTexture(GL_TEXTURE3);
        glBindTexture(GL_TEXTURE_2D, job.wide->getTexture()->m_texID);
        uploadReliefs(job);
        g_pHyprOpenGL->blend(true);
        glBindVertexArray(g_vao);

        const float toFb[4]   = {job.toFb[0], job.toFb[1], job.toFb[2], job.toFb[3]};
        const float toSurf[4] = {toFb[0], toFb[2], toFb[1], toFb[3]}; // a rotation or a flip: its transpose is its inverse

        glUseProgram(prog->id);
        glUniform1i(prog->backdrop, 0);
        glUniform1i(prog->mask, 1);
        glUniform1i(prog->field, 2);
        glUniform1i(prog->wide, 3);
        glUniform1i(prog->reliefs, 4);
        glUniform1i(prog->reliefCount, job.reliefCount);
        glUniform1f(prog->unit, S);
        glUniform2f(prog->surfSize, job.box.w, job.box.h);
        glUniform2f(prog->fbOrigin, job.fbOrigin.x, job.fbOrigin.y);
        glUniformMatrix2fv(prog->toFb, 1, GL_FALSE, toFb);
        glUniformMatrix2fv(prog->toSurf, 1, GL_FALSE, toSurf);
        glUniform2f(prog->bdOrigin, job.copy.x, job.copy.y);
        glUniform2f(prog->bdScale, job.level.x / job.sampled->m_size.x, job.level.y / job.sampled->m_size.y);
        glUniform2f(prog->bdLimit, job.level.x * job.copy.w / job.sampled->m_size.x, job.level.y * job.copy.h / job.sampled->m_size.y);
        glUniform2f(prog->wideScale, job.wideLevel.x / job.wide->m_size.x, job.wideLevel.y / job.wide->m_size.y);
        glUniform2f(prog->wideLimit, job.wideLevel.x * job.copy.w / job.wide->m_size.x, job.wideLevel.y * job.copy.h / job.wide->m_size.y);
        glUniform1f(prog->wideSize, lk.inkBlur * S);
        const auto fs = job.field->m_size;
        glUniform2f(prog->fieldScale, 1.0 / (job.fieldStep * fs.x), 1.0 / (job.fieldStep * fs.y));
        glUniform4f(prog->fieldLimit, 0.5 / fs.x, 0.5 / fs.y, (job.fieldSize.x - 0.5) / fs.x, (job.fieldSize.y - 0.5) / fs.y);
        glUniform1f(prog->fieldStep, job.fieldStep);
        glUniform1f(prog->sigma, job.fieldSigma);
        glUniform1f(prog->band, std::max(lk.band * S, 1e-3F));
        glUniform1f(prog->fade, job.fade);
        glUniform1f(prog->thickness, lk.thickness * S);
        glUniform1f(prog->gap, lk.gap * S);
        glUniform1f(prog->chroma, lk.chroma);
        glUniform1f(prog->rim, lk.rim);
        glUniform1f(prog->tint, lk.tint);
        glUniform1i(prog->ink, job.sdr ? lk.ink : sc<int>(EAVE_GLASS_SURFACE_V1_INK_OWN));
        glUniform1f(prog->contrast, lk.contrast);
        glUniform1f(prog->margin, ISLAND_MARGIN * S);
        glUniform1i(prog->count, job.count);
        glUniform4fv(prog->boxes, MAX_REGIONS, job.boxes.data());
        glUniform4fv(prog->fills, MAX_REGIONS, job.fills.data());
        glUniform1iv(prog->chans, MAX_REGIONS, job.chans.data());
        glUniform1fv(prog->bevels, MAX_REGIONS, job.bevels.data());
        glUniform1i(prog->pictureCount, job.pictureCount);
        glUniform4fv(prog->pictures, MAX_PICTURES, job.pictureBoxes.data());
        glUniform2fv(prog->pictureShapes, MAX_PICTURES, job.pictureShapes.data());
        glUniform2f(prog->fbSize, fbSize.x, fbSize.y);
        glUniform4f(prog->rect, job.fbBox.x, job.fbBox.y, job.fbBox.w, job.fbBox.h);

        glActiveTexture(GL_TEXTURE1);
        job.tex->bind();
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        job.tex->unbind();

        gl.restore();
        glFine("key");
    }

    class CGlassElement : public IPassElement {
      public:
        enum eStep : uint8_t {
            BACKDROP,
            KEY
        };
        CGlassElement(SP<SJob> job, eStep step) : m_job(std::move(job)), m_step(step) {}

        std::vector<UP<IPassElement>> draw(CRenderContext& ctx) override {
            try {
                if (m_step == BACKDROP)
                    takeBackdrop(ctx, *m_job, glassOf(m_job->surface.lock().get()));
                else
                    keyGlass(ctx, *m_job);
            } catch (const std::exception& e) { fail(std::format("drawing: {}", e.what())); } catch (...) {
                fail("drawing: unknown exception");
            }
            return {};
        }
        bool needsLiveBlur(CRenderContext&) override {
            return false;
        }
        bool needsPrecomputeBlur(CRenderContext&) override {
            return false;
        }
        const char* passName() override {
            return "CHyprEaveGlassElement";
        }
        ePassElementType type() override {
            return EK_CUSTOM;
        }

      private:
        SP<SJob> m_job;
        eStep    m_step;
    };

    // Wrap each glass surface in the pass with the two steps, now that the pass is built.
    void wrapLayers(CRenderContext& ctx) {
        auto& rd  = ctx.m_data;
        auto  mon = rd.pMonitor.lock();
        if (!enabled() || !mon)
            return;

        auto&      elements = Internals::passElements(IHyprRenderer::currentPass(ctx));
        using Elem          = std::remove_cvref_t<decltype(elements)>::value_type;
        const bool isSdr    = sdr(rd.currentFB);
        CRegion    drawn;
        for (size_t i = 0; i < elements.size(); i++) {
            if (elements[i].element->type() != EK_SURFACE)
                continue;
            auto* surf = sc<CSurfacePassElement*>(elements[i].element.get());
            auto& d    = surf->m_data;
            if (!d.surface || !d.texture || !d.texture->ok())
                continue;
            auto* glass = glassOf(d.surface.get());
            if (!glass || !hasGlass(glass->m_current))
                continue;

            auto job     = makeShared<SJob>();
            job->mon     = mon;
            job->surface = d.surface;
            job->box     = surf->getTexBox().scale(mon->m_scale).round();
            job->fbBox   = toFbBox(mon, job->box);
            job->tex     = d.texture;
            job->fade    = d.alpha * d.fadeAlpha;
            job->sdr     = isSdr;
            job->state   = glass->m_current;
            // From the surface's logical px to s px: the drawn box over the surface's size, which
            // also covers a buffer still catching up with a resize.
            const auto logical = d.surface->m_current.size;
            job->scale         = logical.x > 0 ? sc<float>(job->box.w / logical.x) : mon->m_scale;
            // Where s's corner and axes land in the framebuffer.
            const auto o  = toFbPoint(mon, job->box.pos());
            const auto ax = toFbPoint(mon, job->box.pos() + Vector2D{1, 0}) - o;
            const auto ay = toFbPoint(mon, job->box.pos() + Vector2D{0, 1}) - o;
            job->fbOrigin = o;
            job->toFb[0]  = ax.x;
            job->toFb[1]  = ax.y;
            job->toFb[2]  = ay.x;
            job->toFb[3]  = ay.y;
            if (d.pLS)
                job->global = CBox{d.pLS->position(Desktop::View::IGeometric::GEOMETRIC_CURRENT), d.pLS->size(Desktop::View::IGeometric::GEOMETRIC_CURRENT)};
            drawn.add(job->box);

            elements.insert(elements.begin() + i, Elem{.elementDamage = {}, .element = makeUnique<CGlassElement>(job, CGlassElement::BACKDROP), .discard = false});
            elements.insert(elements.begin() + i + 2, Elem{.elementDamage = {}, .element = makeUnique<CGlassElement>(job, CGlassElement::KEY), .discard = false});
            i += 2;
        }
        g_monitors[mon.get()].drawn = drawn;
    }

    void removeElements() {
        IHyprRenderer::currentPass(g_pHyprRenderer->context()).removeAllOfType("CHyprEaveGlassElement");
    }

    void forgetMonitor(void* monitor) {
        g_monitors.erase(monitor);
    }

    std::string buffers() {
        std::string out;
        for (auto const& [mon, fbs] : g_monitors) {
            std::vector<SP<IFramebuffer>> all = {fbs.backdrop};
            all.insert(all.end(), fbs.levels.begin(), fbs.levels.end());
            all.push_back(fbs.field);
            all.push_back(fbs.across);
            for (auto const& fb : all) {
                if (!fb || !fb->isAllocated())
                    continue;
                const auto name = NFormatUtils::drmFormatName(fb->m_drmFormat);
                const int  bpp  = name.contains("16") ? 8 : 4;
                out += std::format("  {} {}x{} {:.1f} MiB\n", name, fb->m_size.x, fb->m_size.y, fb->m_size.x * fb->m_size.y * bpp / 1048576.0);
            }
        }
        return out;
    }

    void freeRender() {
        g_monitors.clear();
        for (auto& p : g_glass) {
            if (p.id)
                glDeleteProgram(p.id);
            p = {};
        }
        for (auto* p : {&g_down, &g_up, &g_blur, &g_shape[0], &g_shape[1]}) {
            if (p->id)
                glDeleteProgram(p->id);
            *p = {};
        }
        if (g_vao)
            glDeleteVertexArrays(1, &g_vao);
        if (g_vbo)
            glDeleteBuffers(1, &g_vbo);
        g_vao = g_vbo = 0;
        if (g_reliefs)
            glDeleteTextures(1, &g_reliefs);
        g_reliefs = 0;
    }
}
