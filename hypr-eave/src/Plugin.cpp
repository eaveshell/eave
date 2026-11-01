// hypr-eave's entry points: loading, unloading, its config values, and `hyprctl hypreave`.

#include "Glass.hpp"

#include <dlfcn.h>
#include <sstream>

#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/state/MonitorState.hpp>
#include <hyprland/src/config/values/types/BoolValue.hpp>
#include <hyprland/src/debug/log/Logger.hpp>
#include <hyprland/src/ipc/s1/S1.hpp>

using namespace Render::GL;

namespace Glass {
    HANDLE PHANDLE = nullptr;

    void removeElements();

    static struct {
        CHyprSignalListener               stage, monitorRemoved, configReloaded, mouseMove;
        SP<IPC::Socket1::SCommand>        command;
        SP<Config::Values::CBoolValue>    enabled, cursorUnder;
        bool                              failed = false;
        std::string                       why;
    } g_state;

    void log(const std::string& message) {
        Log::logger->log(Log::ERR, "[hypr-eave] {}", message);
    }

    bool enabled() {
        return !g_state.failed && (!g_state.enabled || g_state.enabled->value());
    }

    bool cursorUnder() {
        return !g_state.cursorUnder || g_state.cursorUnder->value();
    }

    // Every surface hears whether there's glass now, and the screens redraw with or without it.
    static void announce() {
        for (auto* glass : allGlass())
            glass->sendActive(enabled());
        for (auto const& m : State::monitorState()->monitors())
            g_pHyprRenderer->damageMonitor(m);
    }

    void fail(const std::string& why) {
        if (g_state.failed)
            return;
        g_state.failed = true;
        g_state.why    = why;
        log(std::format("glass off until Hyprland restarts: {}", why));
        HyprlandAPI::addNotification(PHANDLE, std::format("[hypr-eave] glass off: {}", why), CHyprColor{1.0, 0.27, 0.23, 1.0}, 8000);
        announce();
    }

    static std::string status() {
        std::string out = std::format("hypr-eave: glass {}{}\n", enabled() ? "on" : "off", g_state.failed ? std::format(" ({})", g_state.why) : "");
        for (auto* glass : allGlass()) {
            const auto& s = glass->m_current;
            const auto& r = glass->m_report;
            size_t      islands = 0, solid = 0;
            for (auto const& region : s.regions)
                (region.kind == EAVE_GLASS_SURFACE_V1_KIND_SOLID ? solid : islands)++;
            out += std::format("surface: {} glass regions, {} solid, {} bezel, {} pictures, {} reliefs; report every {} px: {} reads, {} sent\n", islands, solid,
                               s.bezel.size(), s.pictures.size(), s.reliefs.size(), s.reportCell, r.reads, r.posted);
            out += std::format("  look: band {} thickness {} chroma {} rim {} frost {} depth {} tint {} ink {} contrast {} ink-blur {} gap {}\n", s.look.band, s.look.thickness,
                               s.look.chroma, s.look.rim, s.look.frost, s.look.depth, s.look.tint, s.look.ink, s.look.contrast, s.look.inkBlur, s.look.gap);
        }
        out += cursorStatus();
        const auto bufs = buffers();
        if (!bufs.empty())
            out += "buffers:\n" + bufs;
        return out;
    }
}

using namespace Glass;

APICALL EXPORT std::string PLUGIN_API_VERSION() {
    return HYPRLAND_API_VERSION;
}

APICALL EXPORT PLUGIN_DESCRIPTION_INFO PLUGIN_INIT(HANDLE handle) {
    PHANDLE = handle;

    if (std::string{__hyprland_api_get_hash()} != __hyprland_api_get_client_hash())
        throw std::runtime_error("[hypr-eave] built for a different Hyprland: rebuild it (eave-build)");

    // Stay mapped after an unload: clients may still hold objects whose code is ours (Protocol.cpp).
    Dl_info self;
    if (dladdr(reinterpret_cast<void*>(&PLUGIN_INIT), &self) && self.dli_fname)
        dlopen(self.dli_fname, RTLD_NOW | RTLD_NOLOAD | RTLD_NODELETE);

    g_state = {};
    g_state.enabled = makeShared<Config::Values::CBoolValue>("plugin:hypreave:enabled", "glass behind eave's islands", true);
    if (!HyprlandAPI::addConfigValueV2(PHANDLE, g_state.enabled))
        g_state.enabled.reset();
    g_state.cursorUnder = makeShared<Config::Values::CBoolValue>("plugin:hypreave:cursor_under", "the pointer slides under eave's notch and grown edge", true);
    if (!HyprlandAPI::addConfigValueV2(PHANDLE, g_state.cursorUnder))
        g_state.cursorUnder.reset();

    startProtocol();

    g_state.stage = Event::bus()->m_events.render.stage.listen([](const Event::SRenderStageEvent& event) {
        // Only BEGIN and LAST_MOMENT carry the frame's context, which is all we need it for.
        if (!event.context)
            return;
        auto& ctx = event.context->get();
        try {
            if (event.stage == RENDER_BEGIN)
                onRenderBegin(ctx);
            else if (event.stage == RENDER_LAST_MOMENT)
                wrapLayers(ctx);
        } catch (const std::exception& e) { fail(std::format("render: {}", e.what())); }
        // After the glass: the cursor goes between its backdrop copy and the surface.
        if (event.stage == RENDER_LAST_MOMENT) {
            try {
                tuckCursor(ctx);
            } catch (const std::exception& e) { log(std::format("cursor: {}", e.what())); }
        }
    });
    g_state.mouseMove = Event::bus()->m_events.input.mouse.move.listen([](Vector2D at, Event::SCallbackInfo&) {
        try {
            updateCursorLocks(at);
        } catch (const std::exception& e) { log(std::format("cursor: {}", e.what())); }
    });
    g_state.monitorRemoved = Event::bus()->m_events.monitor.destroyMon.listen([](PHLMONITOR mon) {
        forgetMonitor(mon.get());
        forgetCursorMonitor(mon.get());
    });
    g_state.configReloaded = Event::bus()->m_events.config.reloaded.listen([] {
        announce();
        updateCursorLocks();
    });
    g_state.command        = HyprlandAPI::registerHyprCtlCommand(PHANDLE, IPC::Socket1::SCommand{.name = "hypreave", .handler = [](const IPC::Socket1::SRequest&) {
                                                                      try {
                                                                          return IPC::Socket1::SResponse{status()};
                                                                      } catch (const std::exception& e) { return IPC::Socket1::SResponse{std::format("error: {}\n", e.what())}; }
                                                                  }});

    return {"hypr-eave", "Liquid glass behind eave's islands", "eave", "0.1.0"};
}

APICALL EXPORT void PLUGIN_EXIT() {
    try {
        // A pass is cleared at the start of the next frame: an element left in it would be
        // destroyed later, which is safe (the code stays mapped) but would draw for a frame.
        removeElements();
        g_state.stage.reset();
        g_state.monitorRemoved.reset();
        g_state.configReloaded.reset();
        g_state.mouseMove.reset();
        releaseCursorLocks();
        if (g_state.command)
            HyprlandAPI::unregisterHyprCtlCommand(PHANDLE, g_state.command);
        g_pHyprOpenGL->makeEGLCurrent();
        for (auto* glass : allGlass())
            glass->m_report.release();
        stopProtocol(); // islands that turned light hear the glass is gone, and turn dark
        freeReports();
        freeRender();
        g_state.enabled.reset();
        g_state.cursorUnder.reset();
        g_state.command.reset();
    } catch (const std::exception& e) { log(std::format("unload: {}", e.what())); }
}
