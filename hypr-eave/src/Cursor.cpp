// The pointer under the notch and the grown edge, the way it slides under a MacBook's notch
// (D-113). eave says where they are with bezel records, in every theme.
//
// A software cursor is a texture Hyprland adds to the pass last, right before RENDER_LAST_MOMENT.
// Where it's over a bezel record, a copy of it is drawn right before eave's surface instead, so the
// notch's opaque pixels cover it and its clear ones let it through, at whatever shape the notch has
// this frame. Hyprland's own is clipped to the rest. Each copy is clipped to this frame's damage
// too: a clip region replaces the damage as the scissor.
//
// A hardware cursor is a plane over the whole frame, which nothing can cover. So while the pointer
// is near a bezel record, its monitor draws the cursor in software (a lock Hyprland counts, as
// screen sharing takes), and only then.

#include "Glass.hpp"
#include "Internals.hpp"

#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/render/pass/SurfacePassElement.hpp>
#include <hyprland/src/render/pass/TexPassElement.hpp>
#include <hyprland/src/pointer/PointerManager.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>
#include <hyprland/src/desktop/view/WLSurface.hpp>
#include <hyprland/src/state/MonitorState.hpp>
#include <hyprland/src/config/values/types/BoolValue.hpp>

using namespace Render;

namespace Glass {
    // How near the pointer has to be for its monitor to draw it in software, logical px: a hand
    // ahead of the edge, so the switch happens before the pointer gets there.
    constexpr double NEAR = 16.;

    // The monitors this plugin holds a software cursor lock on: one each at most, so it gives back
    // exactly what it took.
    static auto& g_locked = *new std::unordered_map<void*, PHLMONITORREF>; // never destroyed: see Protocol.cpp

    // A surface's bezel records, logical px on the whole desktop.
    static CRegion bezelGlobal(const CGlassSurface& glass) {
        CRegion out;
        if (glass.m_current.bezel.empty())
            return out;
        const auto surface = glass.m_surface.lock();
        const auto hl      = surface ? Desktop::View::CWLSurface::fromResource(surface) : nullptr;
        const auto global  = hl ? hl->getSurfaceBoxGlobal() : std::nullopt;
        if (!global)
            return out;
        for (auto const& r : glass.m_current.bezel)
            out.add(CBox{global->x + r.x, global->y + r.y, sc<double>(r.w), sc<double>(r.h)});
        return out;
    }

    static void setLock(const PHLMONITOR& mon, bool want) {
        if (g_locked.contains(mon.get()) == want)
            return;
        if (want) {
            g_locked.emplace(mon.get(), mon);
            Pointer::mgr()->lockSoftwareForMonitor(mon);
        } else {
            g_locked.erase(mon.get());
            Pointer::mgr()->unlockSoftwareForMonitor(mon);
        }
        // The software cursor draws, or the hardware one's leftover goes.
        g_pHyprRenderer->damageBox(Pointer::mgr()->getCursorBoxGlobal().expand(4));
    }

    void updateCursorLocks(std::optional<Vector2D> at) {
        // A move event comes before the pointer's position is: the cursor's box, where it's going.
        auto cursor = Pointer::mgr()->getCursorBoxGlobal();
        if (at)
            cursor.translate(*at - Pointer::mgr()->position());
        cursor.expand(NEAR);

        CRegion bezel;
        if (cursorUnder()) {
            for (auto* glass : allGlass())
                bezel.add(bezelGlobal(*glass));
        }
        bezel.intersect(cursor);
        for (auto const& mon : State::monitorState()->monitors())
            setLock(mon, !bezel.copy().intersect(CBox{mon->m_position, mon->m_size}).empty());
    }

    void forgetCursorMonitor(void* monitor) {
        // Its lock goes with it.
        g_locked.erase(monitor);
    }

    void releaseCursorLocks() {
        for (auto const& [_, weak] : g_locked) {
            if (auto mon = weak.lock())
                Pointer::mgr()->unlockSoftwareForMonitor(mon);
        }
        g_locked.clear();
    }

    void tuckCursor(Render::CRenderContext& ctx) {
        if (!cursorUnder())
            return;
        auto&      rd  = ctx.m_data;
        const auto mon = rd.pMonitor.lock();
        const auto tex = Pointer::mgr()->getCurrentCursorTexture();
        if (!mon || !tex)
            return;

        auto& elements = Internals::passElements(IHyprRenderer::currentPass(ctx));
        using Elem     = std::remove_cvref_t<decltype(elements)>::value_type;

        // Hyprland's software cursor: the last texture that's the cursor's. None with a hardware one.
        size_t cursorAt = elements.size();
        for (size_t i = elements.size(); i-- > 0;) {
            if (elements[i].element->type() == EK_TEXTURE && sc<CTexPassElement*>(elements[i].element.get())->m_data.tex == tex) {
                cursorAt = i;
                break;
            }
        }
        if (cursorAt == elements.size())
            return;
        auto*      cursor = sc<CTexPassElement*>(elements[cursorAt].element.get());
        const CBox box    = cursor->m_data.box;

        // Where it's under a bezel record, monitor px, and the first surface it's under.
        CRegion under;
        size_t  surfaceAt = cursorAt;
        for (size_t i = 0; i < cursorAt; i++) {
            if (elements[i].element->type() != EK_SURFACE)
                continue;
            auto*       surf = sc<CSurfacePassElement*>(elements[i].element.get());
            auto const& d    = surf->m_data;
            auto*       glass = d.surface ? glassOf(d.surface.get()) : nullptr;
            if (!glass || glass->m_current.bezel.empty() || !d.mainSurface)
                continue;
            // From the surface's logical px to monitor px, as wrapLayers does.
            const auto texBox  = surf->getTexBox().scale(mon->m_scale).round();
            const auto logical = d.surface->m_current.size;
            const auto k       = logical.x > 0 ? texBox.w / logical.x : mon->m_scale;
            CRegion    here;
            for (auto const& r : glass->m_current.bezel)
                here.add(CBox{texBox.x + r.x * k, texBox.y + r.y * k, r.w * k, r.h * k}.round());
            here.intersect(box);
            if (here.empty())
                continue;
            surfaceAt = std::min(surfaceAt, i);
            under.add(here);
        }
        if (under.empty())
            return;

        const CRegion damage = rd.damage.copy().intersect(box);
        const CRegion below  = damage.copy().intersect(under);
        const CRegion above  = damage.copy().subtract(under);

        // An empty clip region means none at all: a part with nothing to draw goes instead.
        if (above.empty())
            elements.erase(elements.begin() + cursorAt);
        else
            cursor->m_data.clipRegion = above;
        if (!below.empty()) {
            CTexPassElement::SRenderData data;
            data.tex        = tex;
            data.box        = box;
            data.clipRegion = below;
            elements.insert(elements.begin() + surfaceAt, Elem{.elementDamage = {}, .element = makeUnique<CTexPassElement>(std::move(data)), .discard = false});
        }
    }

    std::string cursorStatus() {
        std::string out = std::format("cursor under the notch: {}, software on {} monitor(s)\n", cursorUnder() ? "on" : "off", g_locked.size());
        return out;
    }
}
