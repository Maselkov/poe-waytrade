// <sparkline points="0,33.8,,146.7,..." labels="6 days ago · 37.2 div|...">
//
// A price's last week: a point a day (an empty one is a day with no
// listings, which the line passes over), the area under the line filled,
// and a dot on the last day. Its colour is the element's `color` and its
// size the element's; `opacity` fades it with its row (a stale row).
//   dashed="1":   a dashed, fainter line and no fill, for a thin week
//   baseline="1": a faint dashed line at 0, the week's start, when in range
//
// Hovering it marks the nearest day and shows that day's label instead of
// the row's own text: its nearest ancestor of class "week-scope" gets class
// "reading" and that scope's ".week-reading" element the label. The theme
// swaps ".week-normal" for ".week-reading" in a box of fixed size, so
// nothing moves. Hover state stays in the element: the data model isn't
// touched, and nothing else is rebuilt.
//
// RmlUi has no lines or paths, and the renderer no anti-aliasing: lines are
// quads with a pixel of feathering (to alpha 0) along each side.
#include "sparkline.h"

#include <RmlUi/Core.h>

#include <algorithm>
#include <cmath>
#include <optional>
#include <vector>

namespace {

using Rml::Vector2f;
using Colour = Rml::ColourbPremultiplied;

constexpr float kPi = 3.14159265f;

struct Mesh {
    Rml::Mesh m;

    int vertex(Vector2f p, Colour c) {
        m.vertices.push_back({p, c, {0, 0}});
        return (int)m.vertices.size() - 1;
    }
    void quad(Vector2f a, Colour ca, Vector2f b, Colour cb, Vector2f c, Colour cc, Vector2f d, Colour cd) {
        int i = vertex(a, ca), j = vertex(b, cb), k = vertex(c, cc), l = vertex(d, cd);
        m.indices.insert(m.indices.end(), {i, j, k, i, k, l});
    }
    // A line from a to b, `half` thick each side of it, feathered.
    void segment(Vector2f a, Vector2f b, float half, Colour c) {
        Vector2f d = b - a;
        float len = d.Magnitude();
        if (len < 0.01f) return;
        Vector2f n = Vector2f(-d.y, d.x) * (1.0f / len);
        Colour clear(0, 0, 0, 0);
        quad(a + n * half, c, b + n * half, c, b - n * half, c, a - n * half, c);
        quad(a + n * half, c, b + n * half, c, b + n * (half + 1), clear, a + n * (half + 1), clear);
        quad(a - n * half, c, b - n * half, c, b - n * (half + 1), clear, a - n * (half + 1), clear);
    }
    // The same line in dashes, `on` long with `off` between.
    void dashed(Vector2f a, Vector2f b, float half, Colour c, float on, float off) {
        Vector2f d = b - a;
        float len = d.Magnitude();
        if (len < 0.01f) return;
        Vector2f u = d * (1.0f / len);
        for (float s = 0; s < len; s += on + off) segment(a + u * s, a + u * std::min(s + on, len), half, c);
    }
    // A disc, feathered: also fills the gaps where two segments meet.
    void disc(Vector2f centre, float r, Colour c) {
        constexpr int kSides = 16;
        Colour clear(0, 0, 0, 0);
        int mid = vertex(centre, c);
        for (int i = 0; i < kSides; i++) {
            float a0 = 2 * kPi * (float)i / kSides, a1 = 2 * kPi * (float)(i + 1) / kSides;
            Vector2f e0(std::cos(a0), std::sin(a0)), e1(std::cos(a1), std::sin(a1));
            int p = vertex(centre + e0 * r, c), q = vertex(centre + e1 * r, c);
            m.indices.insert(m.indices.end(), {mid, p, q});
            quad(centre + e0 * r, c, centre + e1 * r, c, centre + e1 * (r + 1), clear, centre + e0 * (r + 1), clear);
        }
    }
};

class ElementSparkline : public Rml::Element, public Rml::EventListener {
public:
    explicit ElementSparkline(const Rml::String& tag) : Rml::Element(tag) {
        AddEventListener(Rml::EventId::Mousemove, this);
        AddEventListener(Rml::EventId::Mouseout, this);
    }
    ~ElementSparkline() override {
        RemoveEventListener(Rml::EventId::Mousemove, this);
        RemoveEventListener(Rml::EventId::Mouseout, this);
    }

    void ProcessEvent(Rml::Event& ev) override {
        if (ev.GetId() == Rml::EventId::Mouseout) return hover(-1);
        float left = GetAbsoluteOffset(Rml::BoxArea::Content).x;
        float x = ev.GetParameter<float>("mouse_x", left) - left;
        // The known day nearest the pointer.
        int best = -1;
        float best_d = 0;
        for (size_t i = 0; i < points_.size(); i++) {
            if (!points_[i]) continue;
            float d = std::fabs(day_x((int)i) - x);
            if (best < 0 || d < best_d) best = (int)i, best_d = d;
        }
        hover(best);
    }

protected:
    void OnAttributeChange(const Rml::ElementAttributes& changed) override {
        Rml::Element::OnAttributeChange(changed);
        if (changed.find("points") != changed.end()) {
            points_.clear();
            Rml::StringList days;
            Rml::StringUtilities::ExpandString(days, GetAttribute<Rml::String>("points", ""), ',', true);
            for (const Rml::String& d : days)
                points_.push_back(d.empty() ? std::nullopt : std::optional<float>(strtof(d.c_str(), nullptr)));
            hovered_ = -1;
        }
        if (changed.find("labels") != changed.end()) {
            labels_.clear();
            Rml::StringUtilities::ExpandString(labels_, GetAttribute<Rml::String>("labels", ""), '|', true);
        }
        dirty_ = true;
    }
    void OnResize() override {
        Rml::Element::OnResize();
        dirty_ = true;
    }
    void OnPropertyChange(const Rml::PropertyIdSet& changed) override {
        Rml::Element::OnPropertyChange(changed);
        if (changed.Contains(Rml::PropertyId::Color) || changed.Contains(Rml::PropertyId::Opacity)) dirty_ = true;
    }
    void OnRender() override {
        if (dirty_) build();
        if (geometry_) geometry_.Render(GetAbsoluteOffset(Rml::BoxArea::Content));
    }

private:
    std::vector<std::optional<float>> points_;
    Rml::StringList labels_;
    int hovered_ = -1;
    bool dirty_ = true;
    Rml::Geometry geometry_;

    float dp() {
        Rml::Context* ctx = GetContext();
        return ctx ? ctx->GetDensityIndependentPixelRatio() : 1.0f;
    }
    Vector2f size() { return GetBox().GetSize(Rml::BoxArea::Content); }
    // Room left at the sides and ends for the line's width and the dot.
    float pad() { return 3 * dp(); }
    float day_x(int i) {
        float w = size().x, p = pad();
        return points_.size() < 2 ? w / 2 : p + (w - 2 * p) * (float)i / (float)(points_.size() - 1);
    }

    void hover(int day) {
        if (day == hovered_) return;
        hovered_ = day;
        dirty_ = true;
        Rml::Element* scope = GetParentNode();
        while (scope && !scope->IsClassSet("week-scope")) scope = scope->GetParentNode();
        if (!scope) return;
        Rml::ElementList reading;
        scope->GetElementsByClassName(reading, "week-reading");
        bool shown = day >= 0 && (size_t)day < labels_.size() && !reading.empty();
        if (shown) reading[0]->SetInnerRML(Rml::StringUtilities::EncodeRml(labels_[(size_t)day]));
        scope->SetClass("reading", shown);
    }

    Colour colour(float alpha) {
        const Rml::ComputedValues& cv = GetComputedValues();
        Rml::Colourb c = cv.color();
        return Rml::Colourb(c.red, c.green, c.blue, (Rml::byte)std::clamp(c.alpha * alpha, 0.0f, 255.0f))
            .ToPremultiplied(cv.opacity());
    }

    void build() {
        dirty_ = false;
        geometry_ = {};
        Rml::RenderManager* rm = GetRenderManager();
        Vector2f box = size();
        std::vector<int> known;
        for (size_t i = 0; i < points_.size(); i++)
            if (points_[i]) known.push_back((int)i);
        if (!rm || known.size() < 2 || box.x <= 0 || box.y <= 0) return;

        float lo = *points_[(size_t)known[0]], hi = lo;
        for (int i : known) lo = std::min(lo, *points_[(size_t)i]), hi = std::max(hi, *points_[(size_t)i]);
        float u = dp(), p = pad(), h = box.y;
        auto at = [&](int i) {
            float v = *points_[(size_t)i];
            float y = hi > lo ? p + (hi - v) / (hi - lo) * (h - 2 * p) : h / 2;
            return Vector2f(day_x(i), y);
        };
        bool dashed = GetAttribute<Rml::String>("dashed", "") == "1";
        Mesh m;

        // The week's start, when the line crosses it.
        if (GetAttribute<Rml::String>("baseline", "") == "1" && lo < 0 && hi > 0) {
            float y = p + hi / (hi - lo) * (h - 2 * p);
            m.dashed({0, y}, {box.x, y}, 0.5f * u, colour(0.3f), 2 * u, 3 * u);
        }
        // The area under the line, then the line, joins rounded.
        if (!dashed) {
            Colour fill = colour(0.12f);
            for (size_t k = 0; k + 1 < known.size(); k++) {
                Vector2f a = at(known[k]), b = at(known[k + 1]);
                m.quad(a, fill, b, fill, {b.x, h}, fill, {a.x, h}, fill);
            }
        }
        Colour line = colour(dashed ? 0.7f : 1.0f);
        float half = 0.75f * u;
        for (size_t k = 0; k + 1 < known.size(); k++) {
            Vector2f a = at(known[k]), b = at(known[k + 1]);
            if (dashed) {
                m.dashed(a, b, half, line, 3 * u, 2 * u);
            } else {
                m.segment(a, b, half, line);
                if (k > 0) m.disc(a, half, line);
            }
        }
        if (dashed) {
            for (int i : known) m.disc(at(i), 1.5f * u, line);
        } else {
            m.disc(at(known.back()), 2 * u, line);
        }
        // The hovered day: a faint rule down to it and a larger dot.
        if (hovered_ >= 0 && (size_t)hovered_ < points_.size() && points_[(size_t)hovered_]) {
            Vector2f d = at(hovered_);
            m.dashed({d.x, 0}, {d.x, h}, 0.5f * u, colour(0.35f), 2 * u, 3 * u);
            m.disc(d, 3 * u, line);
        }
        geometry_ = rm->MakeGeometry(std::move(m.m));
    }
};

Rml::ElementInstancerGeneric<ElementSparkline> sparkline_instancer;

}  // namespace

void register_sparkline() { Rml::Factory::RegisterElementInstancer("sparkline", &sparkline_instancer); }
