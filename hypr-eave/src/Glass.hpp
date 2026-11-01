// hypr-eave: liquid glass behind eave's islands.
//
// eave draws its islands opaque, as it would without this plugin, and tells it over the private
// protocol eave_glass_v1 (protocol/eave-glass-v1.xml) which parts of each frame are islands that
// may turn to glass, in which fill color. Right before such a surface draws, the plugin copies and
// frosts what's on screen around it; right after, it lets that through where the surface is the
// fill color, refracted at the islands' edges. The shape comes only from the buffer drawn this
// frame and what eave said with it, so the glass can't lag a morph. In return the plugin tells
// eave how bright it is behind its surface, so the ink of islands over something bright can turn dark.
//
// eave also says where the notch and the grown edge are, in every theme, and the plugin draws the
// pointer under them, so it slides under the notch as under a MacBook's (Cursor.cpp, D-113).
#pragma once

#include <array>
#include <chrono>
#include <optional>
#include <cstdint>
#include <string>
#include <unordered_map>
#include <vector>

#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/render/OpenGL.hpp>
#include <hyprland/src/render/Framebuffer.hpp>
#include <hyprland/src/output/Monitor.hpp>

#include "eave-glass-v1.hpp"

class CWLSurfaceResource;
namespace Render {
    class CRenderContext;
}

namespace Glass {
    using Clock = std::chrono::steady_clock;

    // How the glass looks: lengths in logical px. eave sends it from its config.toml.
    struct SLook {
        float band      = 24.F;  // how far in from the edge the bevel curves down
        float thickness = 32.F;  // how thick the glass is, behind the bevel: the backdrop bends through it
        float chroma    = 0.2F; // spread between red and blue lookups, as a fraction of the bend
        float rim       = 1.F;    // how bright the edge's reflection and the lit side are
        float frost     = 2.F; // spread of each blur step (0 = clear glass)
        int   depth     = 2;     // how many times the blur halves the backdrop: each doubles its reach
        float tint      = 0.35F; // how much the glass takes the color behind it
        int   ink       = EAVE_GLASS_SURFACE_V1_INK_READABLE;
        float contrast  = 7.F;  // ink readable: the contrast ratio it reaches
        float inkBlur   = 12.F;  // ink: how far apart the taps of the wider blur its color is chosen on are
        float gap       = 24.F;  // how high the glass floats over what's behind it (set_gap, version 3)

        bool  operator==(const SLook&) const = default;
    };

    constexpr size_t MAX_REGIONS = 64;
    // Controls cut into the glass or standing out of it (set_reliefs, version 4, D-126).
    constexpr size_t MAX_RELIEFS = 64;
    // Each relief goes to the shader as this many RGBA texels of its data texture.
    constexpr size_t RELIEF_TEXELS = 4;
    // Pictures on the islands, rounded (set_pictures, version 5, D-129).
    constexpr size_t MAX_PICTURES = 32; // few, so the glass shader's uniforms stay well inside what GLES drivers give
    // libwayland drops a connection that gets a message over 4096 bytes: and eave's is the bar's
    // own. A backdrop event is 24 bytes and its cells, padded to 4.
    constexpr size_t MAX_REPORT_CELLS = 4096 - 24 - 4;

    // A record of eave_glass_surface_v1.set_regions, surface-local logical px.
    struct SRegion {
        int32_t  x = 0, y = 0, w = 0, h = 0;
        uint32_t kind = EAVE_GLASS_SURFACE_V1_KIND_GLASS;
        uint32_t fill = 0xFF000000; // ARGB
        uint8_t  ink  = 0;          // which way grey ink goes: 0 lighter, 255 darker

        bool     operator==(const SRegion&) const = default;
    };

    // A record of eave_glass_surface_v1.set_reliefs: a control on an island, surface-local
    // logical px.
    struct SRelief {
        float    x = 0, y = 0, w = 0, h = 0, radius = 0;
        uint32_t kind  = 0; // 0 a well, 1 a bead
        float    depth = 0; // at rest, logical px
        float    press = 0, milk = 0, lit = 0; // 0 to 1
        float    fillX = 0, lightX = 0, lightY = 0;

        bool     operator==(const SRelief&) const = default;
    };

    // A record of eave_glass_surface_v1.set_pictures: a picture on an island, kept as drawn inside
    // its rounded corners, surface-local logical px.
    struct SPicture {
        float x = 0, y = 0, w = 0, h = 0, radius = 0;
        float opacity = 1; // 0 to 1

        bool  operator==(const SPicture&) const = default;
    };

    // What eave said, double-buffered: requests set the pending one, the surface's commit makes it
    // current.
    struct SState {
        std::vector<SRegion> regions; // glass and solid
        std::vector<SRegion> bezel;   // the notch and the grown edge: the pointer goes under them
        std::vector<SRelief> reliefs; // controls cut into the glass, or standing out of it
        std::vector<SPicture> pictures; // pictures on the islands, rounded
        SLook                look;
        uint32_t             reportCell = 0;

        bool                 operator==(const SState&) const = default;
    };

    // How bright it is behind a surface, read back without stalling: drawn into a small grid, read
    // into a pixel pack buffer, fenced, and polled from a timer.
    struct SReport {
        SP<Render::IFramebuffer> grid;
        GLuint                   pbo   = 0;
        GLsync                   fence = nullptr;
        uint32_t                 cell = 0, cols = 0, rows = 0; // of the read in flight
        Clock::time_point        last;                          // when the last read started
        bool                     missed = false;                // a change came too soon after it
        std::vector<uint8_t>     sent;
        uint32_t                 sentCell = 0, sentCols = 0, sentRows = 0;
        uint64_t                 reads = 0, posted = 0;

        void                     release();
    };

    class CGlassSurface {
      public:
        CGlassSurface(SP<CEaveGlassSurfaceV1> resource, SP<CWLSurfaceResource> surface);
        ~CGlassSurface();

        bool                    good() const;
        void                    sendActive(bool active);
        void                    sendBackdrop(uint32_t cell, uint32_t cols, uint32_t rows, const std::vector<uint8_t>& cells);
        // Unloading: answers nothing from now on, and keeps its resource for the client to destroy.
        void                    makeInert();

        SP<CEaveGlassSurfaceV1> m_resource;
        WP<CWLSurfaceResource>  m_surface;
        SState                  m_pending, m_current;
        std::optional<bool>     m_active; // as last sent
        SReport                 m_report;

      private:
        struct {
            CHyprSignalListener commit, destroy;
        } m_listeners;
    };

    // The surfaces with glass, by their wl_surface.
    CGlassSurface* glassOf(const CWLSurfaceResource* surface);
    std::vector<CGlassSurface*> allGlass();

    // Protocol.cpp
    void startProtocol();
    void stopProtocol(); // unloading: tells every client, removes the global, keeps the rest inert

    // Render.cpp
    void onRenderBegin(Render::CRenderContext& ctx);
    void wrapLayers(Render::CRenderContext& ctx);
    void freeRender();      // GL objects and buffers; the EGL context must be current
    void forgetMonitor(void* monitor);
    std::string buffers(); // for `hyprctl hypreave`

    // One glass surface in this frame's pass: the backdrop is taken just before it draws, and the
    // glass keyed in just after, both at the box and with the buffer it draws.
    //
    // Three spaces: the monitor's (logical px times its scale, where damage lives), the
    // framebuffer's (the same, turned by the monitor's transform: what GL sees), and the
    // surface's, `s` (framebuffer px, but upright, from the surface's top left: what the shaders
    // think in). `fb = fbOrigin + toFb * s`.
    struct SJob {
        PHLMONITORREF            mon;
        WP<CWLSurfaceResource>   surface;
        CBox                     box;   // the surface's texture, monitor px
        CBox                     fbBox; // the same, framebuffer px
        Vector2D                 fbOrigin;
        float                    toFb[4] = {1, 0, 0, 1}; // a mat2, column by column
        float                    scale   = 1.F;          // logical px -> s px
        SP<Render::ITexture>     tex;
        float                    fade = 1.F;
        bool                     sdr  = true; // the ink's contrast math holds (8-bit-ish sRGB)
        SState                   state;
        CBox                     global; // the surface, logical px on the whole desktop

        // What eave said, as the shaders take it (`records`): boxes in s px, each one's fill with
        // its ink in w (-1 for solid), and each glass island's channel of the field (-1 for solid).
        int                                count = 0;
        std::array<float, MAX_REGIONS * 4> boxes{}, fills{};
        std::array<int, MAX_REGIONS>       chans{};
        std::array<float, MAX_REGIONS>     bevels{}; // each island's bevel, s px: the band, or less on a narrow one
        // And the reliefs (`reliefs`), in s px, as the rows of the data texture the glass reads.
        int                                                  reliefCount = 0;
        std::array<float, MAX_RELIEFS * RELIEF_TEXELS * 4>   reliefData{};
        // And the pictures (`pictures`), in s px: each one's box (x0, y0, x1, y1), and its radius
        // and opacity.
        int                                    pictureCount = 0;
        std::array<float, MAX_PICTURES * 4>    pictureBoxes{};
        std::array<float, MAX_PICTURES * 2>    pictureShapes{};

        // Filled by the backdrop step:
        CBox                     copy; // the part of the framebuffer the backdrop holds
        SP<Render::IFramebuffer> sampled;
        Vector2D                 level; // the sampled texture's size over the copy's
        SP<Render::IFramebuffer> wide;  // the most blurred level, for the ink's color
        Vector2D                 wideLevel;
        SP<Render::IFramebuffer> field;          // the surface's coverage, blurred: the bend's shape
        Vector2D                 fieldSize;      // the part of it that holds content, texels
        float                    fieldStep  = 1.F; // s px per texel
        float                    fieldSigma = 1.F; // its gaussian, s px
        bool                     ready     = false;
    };

    // Cursor.cpp: the pointer under the notch and the grown edge.
    bool cursorUnder();              // the config allows it
    void tuckCursor(Render::CRenderContext& ctx); // at RENDER_LAST_MOMENT, after wrapLayers
    void updateCursorLocks(std::optional<Vector2D> at = std::nullopt); // a software cursor only near them
    void forgetCursorMonitor(void* monitor);
    void releaseCursorLocks();       // unloading
    std::string cursorStatus();      // for `hyprctl hypreave`

    // Report.cpp
    void reportBackdrop(CGlassSurface& glass, const SJob& job);
    void freeReports(); // the shared program and timer; the EGL context must be current

    // Plugin.cpp
    bool enabled();        // the config allows it, and nothing failed
    void fail(const std::string& why); // a GL failure: glass off for the session, with one notification
    void log(const std::string& message);

    extern HANDLE PHANDLE;
}
