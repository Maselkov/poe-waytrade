#version 450

layout(push_constant) uniform Push {
    mat4 transform;
    vec2 translate;
    vec2 to_ndc;
    // OutputMode (hdr.h): 0 sRGB values as they are, 1 linear for an _SRGB
    // format, 2 HDR10 (BT.2020, PQ), 3 scRGB (linear BT.709, 1.0 = 80 nits).
    int mode;
    int labels;
    // HDR10: paper white / 10000 nits (PQ's 1.0). scRGB: paper white / 80.
    float hdr_scale;
} pc;

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(location = 0) in vec4 color;
layout(location = 1) in vec2 uv;
layout(location = 0) out vec4 out_color;

vec3 srgb_to_linear(vec3 c) {
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(0.04045, c));
}

// Linear BT.709 primaries to BT.2020's.
vec3 bt709_to_bt2020(vec3 c) {
    return vec3(dot(vec3(0.627404, 0.329283, 0.043313), c), dot(vec3(0.069097, 0.919540, 0.011362), c),
                dot(vec3(0.016391, 0.088013, 0.895595), c));
}

// SMPTE ST 2084 (PQ): linear, 1.0 = 10000 nits, to its encoding.
vec3 pq_encode(vec3 y) {
    const float m1 = 0.1593017578125, m2 = 78.84375;
    const float c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
    vec3 p = pow(clamp(y, 0.0, 1.0), vec3(m1));
    return pow((c1 + c2 * p) / (1.0 + c3 * p), vec3(m2));
}

void main() {
    vec4 c = color * texture(tex, uv);
    // Colours are premultiplied and sRGB-encoded: un-premultiply, convert,
    // premultiply again. The swapchain still blends the encoded values, so
    // edges and translucent panels blend in PQ space under HDR10 (like they
    // do in sRGB space on a UNORM swapchain): not exact, but close enough
    // for a UI.
    if (pc.mode != 0 && c.a > 0.0) {
        vec3 lin = srgb_to_linear(clamp(c.rgb / c.a, 0.0, 1.0));
        if (pc.mode == 1) {
            // An sRGB swapchain encodes on write, so hand it linear colour;
            // otherwise everything comes out washed out.
            c.rgb = lin * c.a;
        } else if (pc.mode == 2) {
            c.rgb = pq_encode(bt709_to_bt2020(lin) * pc.hdr_scale) * c.a;
        } else if (pc.mode == 3) {
            c.rgb = lin * pc.hdr_scale * c.a;
        }
    }
    out_color = c;
}
