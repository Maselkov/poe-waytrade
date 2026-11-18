#version 450

layout(push_constant) uniform Push {
    mat4 transform;
    vec2 translate;
    vec2 to_ndc;
    int srgb;
} pc;

layout(set = 0, binding = 0) uniform sampler2D tex;

layout(location = 0) in vec4 color;
layout(location = 1) in vec2 uv;
layout(location = 0) out vec4 out_color;

vec3 srgb_to_linear(vec3 c) {
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(0.04045, c));
}

void main() {
    vec4 c = color * texture(tex, uv);
    // An sRGB swapchain encodes on write, so hand it linear colour;
    // otherwise everything comes out washed out.
    if (pc.srgb != 0 && c.a > 0.0)
        c.rgb = srgb_to_linear(c.rgb / c.a) * c.a;
    out_color = c;
}
