#include "ui.h"
#include "assets.h"
#include "icons.h"
#include "layer.h"
#include "price_check.h"
#include "rml_renderer.h"

#include <RmlUi/Core.h>

#include <sys/stat.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <map>
#include <sstream>

namespace {

const char* const kPriceDoc = "ui/price_check.rml";
const char* const kMapDoc = "ui/map_check.rml";
const char* const kDebugDoc = "ui/debug.rml";
const char* const kDocuments[] = {kPriceDoc, kMapDoc, kDebugDoc};

// ---- RmlUi interfaces ------------------------------------------------------

class System final : public Rml::SystemInterface {
public:
    double GetElapsedTime() override {
        return std::chrono::duration<double>(std::chrono::steady_clock::now() - start_).count();
    }
    bool LogMessage(Rml::Log::Type type, const Rml::String& message) override {
        if (type <= Rml::Log::LT_WARNING) poe_log("ui: %s", message.c_str());
        return true;
    }
    void SetMouseCursor(const Rml::String& name) override { cursor = name; }

    Rml::String cursor;

private:
    std::chrono::steady_clock::time_point start_ = std::chrono::steady_clock::now();
};

// RCSS has no variables, so theme files declare `@define $name value;` and
// we substitute them when the file is read.
std::string substitute_defines(const std::string& src) {
    std::vector<std::pair<std::string, std::string>> defines;
    std::string out;
    std::istringstream lines(src);
    for (std::string line; std::getline(lines, line);) {
        size_t start = line.find_first_not_of(" \t");
        if (start != std::string::npos && line.compare(start, 8, "@define ") == 0) {
            std::istringstream def(line.substr(start + 8));
            std::string name, value;
            def >> name;
            std::getline(def, value);
            value = value.substr(value.find_first_not_of(" \t") == std::string::npos ? value.size()
                                                                                        : value.find_first_not_of(" \t"));
            while (!value.empty() && (value.back() == ';' || value.back() == ' ' || value.back() == '\r'))
                value.pop_back();
            defines.emplace_back(name, value);
            continue;
        }
        out += line + "\n";
    }
    // Longest names first, so $text doesn't eat part of $text-strong.
    std::sort(defines.begin(), defines.end(), [](auto& a, auto& b) { return a.first.size() > b.first.size(); });
    for (auto& [name, value] : defines)
        for (size_t pos = 0; (pos = out.find(name, pos)) != std::string::npos; pos += value.size())
            out.replace(pos, name.size(), value);
    return out;
}

// Serves assets from POE_WAYTRADE_UI_DIR when set (for live editing),
// otherwise from the copies built into the layer.
class Files final : public Rml::FileInterface {
public:
    Files() {
        if (const char* dir = getenv("POE_WAYTRADE_UI_DIR")) dev_dir = dir;
    }

    std::string dev_dir;

    bool read(const std::string& path, std::string* out) const {
        if (!dev_dir.empty()) {
            std::ifstream f(dev_dir + "/" + path, std::ios::binary);
            if (f) {
                *out = std::string(std::istreambuf_iterator<char>(f), {});
                return true;
            }
        }
        for (size_t i = 0; i < embedded_file_count; i++) {
            if (path == embedded_files[i].path) {
                out->assign((const char*)embedded_files[i].data, embedded_files[i].size);
                return true;
            }
        }
        return false;
    }

    Rml::FileHandle Open(const Rml::String& path) override {
        auto* f = new Open_;
        if (!read(path, &f->data)) {
            delete f;
            return 0;
        }
        if (path.size() > 5 && path.compare(path.size() - 5, 5, ".rcss") == 0) f->data = substitute_defines(f->data);
        return reinterpret_cast<Rml::FileHandle>(f);
    }
    void Close(Rml::FileHandle h) override { delete reinterpret_cast<Open_*>(h); }
    size_t Read(void* buffer, size_t size, Rml::FileHandle h) override {
        auto* f = reinterpret_cast<Open_*>(h);
        size = std::min(size, f->data.size() - f->pos);
        memcpy(buffer, f->data.data() + f->pos, size);
        f->pos += size;
        return size;
    }
    bool Seek(Rml::FileHandle h, long offset, int origin) override {
        auto* f = reinterpret_cast<Open_*>(h);
        long base = origin == SEEK_SET ? 0 : origin == SEEK_CUR ? (long)f->pos : (long)f->data.size();
        long pos = base + offset;
        if (pos < 0 || pos > (long)f->data.size()) return false;
        f->pos = pos;
        return true;
    }
    size_t Tell(Rml::FileHandle h) override { return reinterpret_cast<Open_*>(h)->pos; }

private:
    struct Open_ {
        std::string data;
        size_t pos = 0;
    };
};

// ---- the data model ---------------------------------------------------------

struct UiPrice {
    Rml::String label, main, sub;
};
struct UiFilter {
    int index = 0;
    bool enabled = false;
    Rml::String label, min;
    Rml::String bounds;            // "185+" or "185–220"
    float min_pct = 0, max_pct = 100;  // handle positions on the slider
    bool has_slider = false, max_roll = false;
    bool visible = true;  // extra mods fold away until "+ N more"
};
struct UiListing {
    Rml::String price, sub, age, count, icon;
};

struct UiMapStat {
    Rml::String label, value;
};
struct UiMapMod {
    int index = 0;      // into the reply's mods, for cycle_mod
    Rml::String text;
    Rml::String mark;   // deadly | warning | good | "" (CSS class)
    Rml::String tag;    // Deadly | Warning | Good | ""
};

struct Model {
    Rml::String name, base, subtitle, rarity;
    Rml::String state;  // status | prices | trade
    Rml::String status, source, thin, total, hint;
    bool status_error = false, searching = false, has_url = false, closable = false;
    // Trade search options
    Rml::String base_label, category_label, corrupted, corrupted_label;
    bool base_only = false, searched = false, stale = false;
    Rml::Vector<UiPrice> prices;
    Rml::Vector<UiFilter> weapon_filters, mod_filters;
    Rml::Vector<UiListing> listings;
    int more_mods = 0;  // folded away
    // Price summary (median of the fetched listings)
    bool has_summary = false, low_value = false;
    Rml::String summary_price, summary_icon, summary_sub, summary_range;
    Rml::Vector<Rml::String> debug_lines;
    // Map check
    Rml::String verdict, verdict_kind;  // kind: deadly | warning | clean
    Rml::Vector<UiMapStat> map_stats;
    Rml::Vector<UiMapMod> map_mods;
    int profile = 1;
};

Rml::String format(const char* fmt, double v) {
    char buf[64];
    snprintf(buf, sizeof(buf), fmt, v);
    return buf;
}

Rml::String chaos_text(double chaos) { return format(chaos < 10 ? "%.1fc" : "%.0fc", chaos); }

// "2184" -> "2,184"
Rml::String thousands(const std::string& digits) {
    if (digits.empty() || digits.find_first_not_of("0123456789") != std::string::npos) return digits;
    std::string out;
    int n = 0;
    for (auto it = digits.rbegin(); it != digits.rend(); ++it, ++n) {
        if (n && n % 3 == 0) out.insert(out.begin(), ',');
        out.insert(out.begin(), *it);
    }
    return out;
}

int count_listings(const PriceReply& reply) {
    int n = 0;
    for (const TradeListing& l : reply.listings) n += l.count;
    return n;
}

Rml::String rarity_class(const std::string& rarity) {
    if (rarity == "Divination Card") return "card";
    Rml::String r = rarity;
    std::transform(r.begin(), r.end(), r.begin(), ::tolower);
    return r;
}

time_t mtime(const std::string& path) {
    struct stat st;
    return stat(path.c_str(), &st) == 0 ? st.st_mtime : 0;
}

}  // namespace

struct OverlayUi::Impl {
    System system;
    Files files;
    Rml::Context* context = nullptr;
    std::map<std::string, Rml::ElementDocument*> docs;
    Model model;
    Rml::DataModelHandle handle;
    PriceCheck* price_check = nullptr;  // during update(), for event callbacks
    uint64_t revision = UINT64_MAX;
    // "+ N more mods" expanded, for this check only.
    bool show_all_mods = false;
    bool range_max_handle = false;  // which handle the current slider drag moves

    void range_input(int index, Rml::Event& ev, bool press) {
        if (!price_check || !price_check->reply()) return;
        const TradeFilter* filter = nullptr;
        for (const TradeFilter& f : price_check->reply()->filters)
            if (f.index == index) filter = &f;
        if (!filter || filter->hi <= filter->lo) return;
        Rml::Element* track = ev.GetCurrentElement();
        float left = track->GetAbsoluteOffset(Rml::BoxArea::Content).x;
        float width = track->GetBox().GetSize(Rml::BoxArea::Content).x;
        if (width <= 0) return;
        float x = ev.GetParameter<float>("mouse_x", left);
        double lo = filter->lo, hi = filter->hi, step = filter->step > 0 ? filter->step : 1;
        double v = lo + std::clamp((x - left) / width, 0.0f, 1.0f) * (hi - lo);
        v = std::clamp(std::round(v / step) * step, lo, hi);
        double min = filter->min.empty() ? lo : atof(filter->min.c_str());
        double max = filter->max.empty() ? -1 : atof(filter->max.c_str());
        double max_at = max < 0 ? hi : max;
        if (press) {
            double to_min = std::abs(v - std::min(min, hi)), to_max = std::abs(v - max_at);
            range_max_handle = to_max < to_min || (to_max == to_min && v > max_at - 1e-9 && v >= min);
        }
        if (range_max_handle)
            max = v >= hi ? -1 : std::max(v, std::min(min, hi));
        else
            min = std::min(v, max_at);
        price_check->set_bounds(index, min, max);
    }
    uint64_t shown_all_check = 0;
    uint64_t placed_check = 0;
    bool debug = getenv("POE_WAYTRADE_DEBUG") != nullptr;
    float ui_scale = 1.0f;
    int frame = 0;
    time_t loaded_mtime = 0;
    bool ok = false;

    bool init(RmlRenderer* renderer);
    void bind_model();
    void load_documents();
    time_t dev_files_mtime() const;
    void sync(PriceCheck& pc);
    void sync_debug(WaylandInput* input, VkExtent2D extent);
    void place(PriceCheck& pc, VkExtent2D extent);
    bool keep_on_screen(VkExtent2D extent);
    void sync_map(const PriceReply& reply, uint64_t check_id, Model& m);
    void fill_prices(const PriceReply& reply, Model& m);
    // Map mod display order, fixed when a check opens so rows don't jump
    // around while you mark them.
    uint64_t map_order_check = 0;
    int map_order_profile = 0;
    std::vector<int> map_order;
    // The panel document for the current check.
    const char* panel_doc(const PriceCheck& pc) const {
        return pc.purpose() == PriceCheck::Purpose::Map ? kMapDoc : kPriceDoc;
    }
    const char* shown_panel = kPriceDoc;
};

OverlayUi::OverlayUi(RmlRenderer* renderer) : impl_(std::make_unique<Impl>()) {
    impl_->ok = impl_->init(renderer);
}

OverlayUi::~OverlayUi() {
    if (impl_->ok) Rml::Shutdown();
}

bool OverlayUi::ok() const { return impl_->ok; }

bool OverlayUi::Impl::init(RmlRenderer* renderer) {
    if (const char* s = getenv("POE_WAYTRADE_UI_SCALE")) ui_scale = std::max(0.25f, (float)atof(s));
    Rml::SetRenderInterface(renderer);
    Rml::SetSystemInterface(&system);
    Rml::SetFileInterface(&files);
    if (!Rml::Initialise()) return false;

    const struct {
        const char* path;
        const char* family;
        Rml::Style::FontWeight weight;
    } fonts[] = {
        {"fonts/Inter-Regular.ttf", "Inter", Rml::Style::FontWeight::Normal},
        {"fonts/Inter-Medium.ttf", "Inter", Rml::Style::FontWeight(500)},
        {"fonts/Cinzel.ttf", "Cinzel", Rml::Style::FontWeight::Normal},
    };
    for (auto& f : fonts) {
        for (size_t i = 0; i < embedded_file_count; i++) {
            if (strcmp(embedded_files[i].path, f.path) != 0) continue;
            Rml::LoadFontFace({(const Rml::byte*)embedded_files[i].data, embedded_files[i].size}, f.family,
                              Rml::Style::FontStyle::Normal, f.weight, false);
        }
    }

    context = Rml::CreateContext("overlay", Rml::Vector2i(1920, 1080));
    if (!context) return false;
    bind_model();
    load_documents();
    return !docs.empty();
}

void OverlayUi::Impl::bind_model() {
    Rml::DataModelConstructor c = context->CreateDataModel("ui");
    if (auto h = c.RegisterStruct<UiPrice>()) {
        h.RegisterMember("label", &UiPrice::label);
        h.RegisterMember("main", &UiPrice::main);
        h.RegisterMember("sub", &UiPrice::sub);
    }
    if (auto h = c.RegisterStruct<UiFilter>()) {
        h.RegisterMember("index", &UiFilter::index);
        h.RegisterMember("enabled", &UiFilter::enabled);
        h.RegisterMember("label", &UiFilter::label);
        h.RegisterMember("min", &UiFilter::min);
        h.RegisterMember("bounds", &UiFilter::bounds);
        h.RegisterMember("min_pct", &UiFilter::min_pct);
        h.RegisterMember("max_pct", &UiFilter::max_pct);
        h.RegisterMember("has_slider", &UiFilter::has_slider);
        h.RegisterMember("max_roll", &UiFilter::max_roll);
        h.RegisterMember("visible", &UiFilter::visible);
    }
    if (auto h = c.RegisterStruct<UiListing>()) {
        h.RegisterMember("price", &UiListing::price);
        h.RegisterMember("sub", &UiListing::sub);
        h.RegisterMember("age", &UiListing::age);
        h.RegisterMember("count", &UiListing::count);
        h.RegisterMember("icon", &UiListing::icon);
    }
    c.RegisterArray<Rml::Vector<UiPrice>>();
    c.RegisterArray<Rml::Vector<UiFilter>>();
    c.RegisterArray<Rml::Vector<UiListing>>();
    c.RegisterArray<Rml::Vector<Rml::String>>();
    if (auto h = c.RegisterStruct<UiMapStat>()) {
        h.RegisterMember("label", &UiMapStat::label);
        h.RegisterMember("value", &UiMapStat::value);
    }
    if (auto h = c.RegisterStruct<UiMapMod>()) {
        h.RegisterMember("index", &UiMapMod::index);
        h.RegisterMember("text", &UiMapMod::text);
        h.RegisterMember("mark", &UiMapMod::mark);
        h.RegisterMember("tag", &UiMapMod::tag);
    }
    c.RegisterArray<Rml::Vector<UiMapStat>>();
    c.RegisterArray<Rml::Vector<UiMapMod>>();

    c.Bind("name", &model.name);
    c.Bind("base", &model.base);
    c.Bind("subtitle", &model.subtitle);
    c.Bind("rarity", &model.rarity);
    c.Bind("state", &model.state);
    c.Bind("status", &model.status);
    c.Bind("status_error", &model.status_error);
    c.Bind("source", &model.source);
    c.Bind("thin", &model.thin);
    c.Bind("total", &model.total);
    c.Bind("hint", &model.hint);
    c.Bind("closable", &model.closable);
    c.Bind("base_label", &model.base_label);
    c.Bind("category_label", &model.category_label);
    c.Bind("corrupted", &model.corrupted);
    c.Bind("corrupted_label", &model.corrupted_label);
    c.Bind("base_only", &model.base_only);
    c.Bind("searched", &model.searched);
    c.Bind("stale", &model.stale);
    c.Bind("verdict", &model.verdict);
    c.Bind("verdict_kind", &model.verdict_kind);
    c.Bind("map_stats", &model.map_stats);
    c.Bind("map_mods", &model.map_mods);
    c.Bind("profile", &model.profile);
    c.Bind("searching", &model.searching);
    c.Bind("has_url", &model.has_url);
    c.Bind("prices", &model.prices);
    c.Bind("weapon_filters", &model.weapon_filters);
    c.Bind("mod_filters", &model.mod_filters);
    c.Bind("more_mods", &model.more_mods);
    c.Bind("listings", &model.listings);
    c.Bind("has_summary", &model.has_summary);
    c.Bind("low_value", &model.low_value);
    c.Bind("summary_price", &model.summary_price);
    c.Bind("summary_icon", &model.summary_icon);
    c.Bind("summary_sub", &model.summary_sub);
    c.Bind("summary_range", &model.summary_range);
    c.Bind("debug_lines", &model.debug_lines);

    // Events arrive while update() feeds pointer input, with price_check set.
    c.BindEventCallback("close", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->close();
    });
    c.BindEventCallback("toggle_base", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check && price_check->reply()) price_check->set_base_only(!price_check->reply()->base_only);
    });
    c.BindEventCallback("cycle_corrupted", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->cycle_corrupted();
    });
    c.BindEventCallback("search", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->search();
    });
    // Range sliders: a press moves the nearer handle there, dragging moves it on.
    c.BindEventCallback("range_press", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        if (!args.empty()) range_input(args[0].Get<int>(), ev, true);
    });
    c.BindEventCallback("range_drag", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        if (!args.empty()) range_input(args[0].Get<int>(), ev, false);
    });
    c.BindEventCallback("show_more_mods", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (!price_check) return;
        show_all_mods = true;
        shown_all_check = price_check->check_id();
        revision = UINT64_MAX;  // rebuild the lists
    });
    c.BindEventCallback("toggle_filter", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList& args) {
        if (price_check && !args.empty()) price_check->toggle_filter(args[0].Get<int>());
    });
    // Bound to mousedown: left button cycles forward, right button back.
    c.BindEventCallback("cycle_mod", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        int button = ev.GetParameter<int>("button", 0);
        if (price_check && !args.empty() && button <= 1) price_check->cycle_mod(args[0].Get<int>(), button == 1);
    });
    c.BindEventCallback("set_profile", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList& args) {
        if (price_check && !args.empty()) price_check->set_profile(args[0].Get<int>());
    });
    c.BindEventCallback("open_trade", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->open_trade();
    });
    handle = c.GetModelHandle();
}

void OverlayUi::Impl::load_documents() {
    for (auto& [path, doc] : docs) doc->Close();
    docs.clear();
    Rml::Factory::ClearStyleSheetCache();
    Rml::Factory::ClearTemplateCache();
    for (const char* path : kDocuments) {
        if (auto* doc = context->LoadDocument(path))
            docs[path] = doc;
        else
            poe_log("ui: couldn't load %s", path);
    }
    loaded_mtime = dev_files_mtime();
    revision = UINT64_MAX;  // re-sync visibility
}

time_t OverlayUi::Impl::dev_files_mtime() const {
    if (files.dev_dir.empty()) return 0;
    time_t newest = mtime(files.dev_dir + "/ui/theme.rcss");
    for (const char* path : kDocuments) newest = std::max(newest, mtime(files.dev_dir + "/" + path));
    return newest;
}

void OverlayUi::update(PriceCheck& pc, WaylandInput* input, VkExtent2D extent) {
    Impl& d = *impl_;
    if (!d.ok) return;
    d.frame++;

    // Live editing: reload documents when their files change.
    if (!d.files.dev_dir.empty() && d.frame % 60 == 0 && d.dev_files_mtime() != d.loaded_mtime) {
        poe_log("ui: reloading documents");
        d.load_documents();
    }

    d.context->SetDimensions(Rml::Vector2i((int)extent.width, (int)extent.height));
    d.context->SetDensityIndependentPixelRatio((input ? (float)input->scale() : 1.0f) * d.ui_scale);

    d.price_check = &pc;
    if (input) {
        for (const PointerEvent& ev : input->take_pointer_events()) {
            switch (ev.type) {
            case PointerEvent::Motion: d.context->ProcessMouseMove((int)ev.x, (int)ev.y, 0); break;
            case PointerEvent::Leave: d.context->ProcessMouseLeave(); break;
            case PointerEvent::Button:
                ev.down ? d.context->ProcessMouseButtonDown(ev.button, 0)
                        : d.context->ProcessMouseButtonUp(ev.button, 0);
                break;
            case PointerEvent::Wheel: d.context->ProcessMouseWheel(Rml::Vector2f(0, -ev.wheel), 0); break;
            }
        }
    }
    d.price_check = nullptr;

    d.sync(pc);
    if (d.debug && d.frame % 30 == 0) d.sync_debug(input, extent);

    for (const char* path : {kPriceDoc, kMapDoc}) {
        auto it = d.docs.find(path);
        if (it == d.docs.end()) continue;
        bool want = pc.visible() && path == d.panel_doc(pc);
        if (want && !it->second->IsVisible()) it->second->Show(Rml::ModalFlag::None, Rml::FocusFlag::None);
        if (!want && it->second->IsVisible()) it->second->Hide();
    }
    if (auto it = d.docs.find(kDebugDoc); it != d.docs.end()) {
        if (d.debug && !it->second->IsVisible()) it->second->Show(Rml::ModalFlag::None, Rml::FocusFlag::None);
    }

    d.place(pc, extent);
    d.context->Update();
    // Keeping the panel on screen needs its laid-out size; redo the layout
    // if that moved it.
    if (d.keep_on_screen(extent)) d.context->Update();
    if (input) input->set_cursor(d.system.cursor);
}

void OverlayUi::render() {
    if (impl_->ok) impl_->context->Render();
}

std::vector<InputRect> OverlayUi::input_rects() const {
    std::vector<InputRect> rects;
    if (!impl_->ok) return rects;
    for (auto& [path, doc] : impl_->docs) {
        if (!doc->IsVisible()) continue;
        Rml::Vector2f pos = doc->GetAbsoluteOffset(Rml::BoxArea::Border);
        Rml::Vector2f size = doc->GetBox().GetSize(Rml::BoxArea::Border);
        rects.push_back({(int)floorf(pos.x), (int)floorf(pos.y), (int)ceilf(size.x), (int)ceilf(size.y)});
    }
    return rects;
}

void OverlayUi::Impl::sync(PriceCheck& pc) {
    if (pc.revision() == revision && pc.searching() == model.searching) return;
    revision = pc.revision();

    Model m;
    m.debug_lines = std::move(model.debug_lines);
    m.searching = pc.searching();
    bool map = pc.purpose() == PriceCheck::Purpose::Map;
    // Only a locked panel (Ctrl+Alt+D, Alt+D) needs a close button: the
    // others close by themselves when the mouse moves away.
    m.closable = pc.mode() == PriceCheck::Mode::Locked;
    switch (pc.mode()) {
    case PriceCheck::Mode::Quick: m.hint = "Hold Ctrl and move here to use filters"; break;
    // Same line count as in Quick, so pinning doesn't resize the panel
    // (and pull it out from under the pointer).
    case PriceCheck::Mode::Pinned: m.hint = "Release Ctrl and move away to close"; break;
    case PriceCheck::Mode::Locked: if (map) m.hint = "Click a mod to mark it · right-click to go back"; break;
    }
    const auto& item = pc.item();
    const auto& reply = pc.reply();

    if (item) {
        m.name = item->name;
        m.base = item->base;
        m.rarity = rarity_class(item->rarity);
        // "Rare · Rings", but just "Stackable Currency" / "Skill Gems".
        bool redundant = item->rarity.empty() || item->item_class.find(item->rarity) != std::string::npos;
        m.subtitle = redundant ? item->item_class : item->rarity + " · " + item->item_class;
        if (!item->item_level.empty()) m.subtitle += " · ilvl " + item->item_level;
    }

    if (!item || !reply) {
        m.state = "status";
        m.status = !pc.status().empty() ? pc.status()
                   : m.searching         ? (map ? "Checking map…" : "Checking price…")
                                         : (map ? "No map check" : "No price");
        m.status_error = !pc.status().empty();
    } else if (reply->status == "map") {
        m.state = "map";
        if (!reply->subtitle.empty()) m.subtitle = reply->subtitle;
        sync_map(*reply, pc.check_id(), m);
    } else if (reply->status == "not_map") {
        m.state = "status";
        m.status = "Not a map. Alt+D checks maps, invitations, contracts and logbooks.";
    } else if (reply->status == "trade") {
        m.state = "trade";
        m.status = pc.status();
        m.total = reply->total;
        m.has_url = !reply->url.empty();
        m.searched = reply->searched;
        m.stale = pc.stale();
        m.base_only = reply->base_only;
        m.base_label = reply->base_type;
        if (!reply->category_label.empty()) m.category_label = "Any " + reply->category_label;
        // Short, so the button keeps its size; its colour tells the state too.
        m.corrupted = reply->corrupted;
        m.corrupted_label = reply->corrupted == "no" ? "Uncorrupted" : reply->corrupted == "yes" ? "Corrupted" : "Any";
        m.source = reply->league;
        // A unique's poe.ninja price, which doesn't know its rolls.
        fill_prices(*reply, m);
        // Weapon DPS first; totals and ticked mods always shown, the rest
        // folded behind "+ N more mods" until expanded.
        if (pc.check_id() != shown_all_check) show_all_mods = false;
        for (const TradeFilter& f : reply->filters) {
            UiFilter u{f.index, f.enabled, f.label, f.min};
            u.has_slider = f.hi > f.lo && !f.min.empty();
            if (u.has_slider) {
                auto pct = [&](double v) { return (float)std::clamp((v - f.lo) / (f.hi - f.lo) * 100, 0.0, 100.0); };
                u.min_pct = pct(atof(f.min.c_str()));
                u.max_pct = f.max.empty() ? 100 : pct(atof(f.max.c_str()));
            }
            if (!f.min.empty()) u.bounds = f.min + (f.max.empty() ? "+" : "\u2013" + f.max);
            else if (!f.max.empty()) u.bounds = "\u2264 " + f.max;
            u.max_roll = f.max_roll;
            if (f.section == "weapon") {
                m.weapon_filters.push_back(u);
                continue;
            }
            u.visible = f.section == "total" || f.section == "roll" || f.enabled || show_all_mods;
            if (!u.visible) m.more_mods++;
            m.mod_filters.push_back(u);
        }
        for (const TradeListing& l : reply->listings) {
            UiListing u;
            u.price = l.amount + " " + l.currency;
            // Skip conversions too small to read ("0.0c").
            if (l.chaos >= 0.1 && l.currency != "Chaos Orb") u.sub = chaos_text(l.chaos);
            u.age = l.age;
            if (l.count > 1) u.count = "×" + std::to_string(l.count);
            if (icons_get(l.currency_id)) u.icon = l.currency_id;
            m.listings.push_back(u);
        }
        if (reply->summary) {
            const TradeSummary& s = *reply->summary;
            m.has_summary = true;
            m.summary_price = "≈ " + s.amount + " " + s.currency;
            if (icons_get(s.currency_id)) m.summary_icon = s.currency_id;
            if (s.chaos >= 0.1 && s.currency != "Chaos Orb") m.summary_sub = chaos_text(s.chaos);
            m.low_value = s.chaos >= 0 && s.chaos < 5;
            m.summary_range = std::to_string(count_listings(*reply)) + " of " + thousands(reply->total) + " · " + s.range;
        }
    } else if (reply->status == "ok") {
        m.state = "prices";
        fill_prices(*reply, m);
        Rml::String listed = reply->prices.empty() ? "" : reply->prices[0].listings;
        bool thin = !listed.empty() && atoi(listed.c_str()) < 5;
        if (thin) m.thin = listed + " listed · thin";
        m.source = (!listed.empty() && !thin ? listed + " listed · " : "") + "poe.ninja · " + reply->league;
        if (reply->age_minutes >= 0) m.source += " · " + std::to_string(reply->age_minutes) + " min ago";
    } else {
        m.state = "status";
        m.status = !reply->message.empty() ? reply->message : reply->status;
        m.status_error = reply->status == "error";
    }

    model = std::move(m);
    handle.DirtyAllVariables();
}

// Like Awakened PoE Trade: an item checked in the inventory (right half)
// gets the panel right against the inventory's left edge, one in the stash
// (left half) right against the stash's right edge. PoE's side panels are
// 986/1600 of the window height wide.
void OverlayUi::Impl::place(PriceCheck& pc, VkExtent2D extent) {
    shown_panel = panel_doc(pc);
    if (pc.check_id() == placed_check) return;
    placed_check = pc.check_id();
    auto it = docs.find(shown_panel);
    float ax, ay;
    if (it == docs.end() || !pc.anchor(&ax, &ay)) return;
    float panel = extent.height * 986.0f / 1600.0f;
    float width = 300 * context->GetDensityIndependentPixelRatio();
    float left = ax > extent.width / 2.0f ? extent.width - panel - width : panel;
    it->second->SetProperty(Rml::PropertyId::Left, Rml::Property(left, Rml::Unit::PX));
    it->second->SetProperty(Rml::PropertyId::Top, Rml::Property(extent.height * 0.08f, Rml::Unit::PX));
}

bool OverlayUi::Impl::keep_on_screen(VkExtent2D extent) {
    auto it = docs.find(shown_panel);
    if (it == docs.end() || !it->second->IsVisible()) return false;
    Rml::ElementDocument* doc = it->second;
    float margin = 16 * context->GetDensityIndependentPixelRatio();
    Rml::Vector2f pos = doc->GetAbsoluteOffset(Rml::BoxArea::Border);
    Rml::Vector2f size = doc->GetBox().GetSize(Rml::BoxArea::Border);
    float x = std::clamp(pos.x, margin, std::max(margin, extent.width - margin - size.x));
    float y = std::clamp(pos.y, margin, std::max(margin, extent.height - margin - size.y));
    if (std::abs(x - pos.x) < 0.5f && std::abs(y - pos.y) < 0.5f) return false;
    doc->SetProperty(Rml::PropertyId::Left, Rml::Property(x, Rml::Unit::PX));
    doc->SetProperty(Rml::PropertyId::Top, Rml::Property(y, Rml::Unit::PX));
    return true;
}

// Marks for the active profile, dangerous mods first, and a one-line verdict
// readable at a glance. Both stay one line/one order while the panel is open,
// so marking a mod doesn't move anything.
void OverlayUi::Impl::fill_prices(const PriceReply& reply, Model& m) {
    for (const PriceLine& p : reply.prices) {
        UiPrice u;
        u.label = p.label;
        if (p.divine >= 1.0) {
            u.main = format(p.divine >= 10 ? "%.0f div" : "%.1f div", p.divine);
            u.sub = chaos_text(p.chaos);
        } else {
            u.main = chaos_text(p.chaos);
        }
        m.prices.push_back(u);
    }
}

void OverlayUi::Impl::sync_map(const PriceReply& reply, uint64_t check_id, Model& m) {
    m.profile = std::clamp(reply.profile, 1, 3);
    for (const MapStat& s : reply.map_stats) {
        Rml::String label = s.label;
        if (label == "Item Quantity") label = "Quantity";
        else if (label == "Item Rarity") label = "Rarity";
        else if (label == "Monster Pack Size") label = "Pack size";
        m.map_stats.push_back({label, s.value});
    }

    int deadly = 0, warnings = 0, good = 0;
    std::vector<UiMapMod> mods;
    for (size_t i = 0; i < reply.map_mods.size(); i++) {
        const MapMod& mod = reply.map_mods[i];
        char mark = (size_t)(m.profile - 1) < mod.marks.size() ? mod.marks[m.profile - 1] : '-';
        UiMapMod u{(int)i, mod.text, "", ""};
        if (mark == 'd') u.mark = "deadly", u.tag = "Deadly", deadly++;
        else if (mark == 'w') u.mark = "warning", u.tag = "Warning", warnings++;
        else if (mark == 'g') u.mark = "good", u.tag = "Good", good++;
        mods.push_back(u);
    }

    if (check_id != map_order_check || m.profile != map_order_profile || map_order.size() != mods.size()) {
        map_order_check = check_id;
        map_order_profile = m.profile;
        auto rank = [](const UiMapMod& u) { return u.mark == "deadly" ? 0 : u.mark == "warning" ? 1 : u.mark == "good" ? 2 : 3; };
        std::vector<UiMapMod> sorted = mods;
        std::stable_sort(sorted.begin(), sorted.end(),
                         [&](const UiMapMod& a, const UiMapMod& b) { return rank(a) < rank(b); });
        map_order.clear();
        for (const UiMapMod& u : sorted) map_order.push_back(u.index);
    }
    for (int i : map_order) m.map_mods.push_back(mods[i]);

    auto count = [](int n, const char* one, const char* many) { return std::to_string(n) + " " + (n == 1 ? one : many); };
    if (deadly || warnings) {
        m.verdict_kind = deadly ? "deadly" : "warning";
        if (deadly) m.verdict = count(deadly, "deadly mod", "deadly mods");
        if (warnings) m.verdict += (deadly ? " · " : "") + count(warnings, "warning", "warnings");
    } else {
        m.verdict_kind = "clean";
        m.verdict = reply.map_mods.empty() ? "No mods" : "No dangerous mods";
    }
    if (good) m.verdict += " · " + count(good, "good mod", "good mods");
}

void OverlayUi::Impl::sync_debug(WaylandInput* input, VkExtent2D extent) {
    Rml::Vector<Rml::String> lines;
    char buf[160];
    snprintf(buf, sizeof(buf), "Swapchain %ux%u", extent.width, extent.height);
    lines.push_back(buf);
    if (input) {
        snprintf(buf, sizeof(buf), "Wayland: %s, focus %s", input->status(), input->focused() ? "yes" : "no");
        lines.push_back(buf);
        snprintf(buf, sizeof(buf), "Last key %u %s", input->last_key(), input->last_key_down() ? "down" : "up");
        lines.push_back(buf);
        snprintf(buf, sizeof(buf), "Clipboard #%llu, %zu bytes", (unsigned long long)input->clipboard_seq(),
                 input->clipboard_text().size());
        lines.push_back(buf);
        snprintf(buf, sizeof(buf), "Pointer over overlay: %s, scale %.3f", input->pointer_inside() ? "yes" : "no",
                 input->scale());
        lines.push_back(buf);
    } else {
        lines.push_back("No Wayland input (X11 isn't supported)");
    }
    if (lines != model.debug_lines) {
        model.debug_lines = std::move(lines);
        handle.DirtyVariable("debug_lines");
    }
}
