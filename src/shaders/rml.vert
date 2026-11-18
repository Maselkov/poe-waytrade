#version 450

layout(push_constant) uniform Push {
    mat4 transform;
    vec2 translate;
    vec2 to_ndc;  // 2 / viewport size
    int srgb;
} pc;

layout(location = 0) in vec2 in_pos;
layout(location = 1) in vec4 in_color;  // premultiplied, sRGB-encoded
layout(location = 2) in vec2 in_uv;

layout(location = 0) out vec4 color;
layout(location = 1) out vec2 uv;

void main() {
    vec4 p = pc.transform * vec4(in_pos + pc.translate, 0.0, 1.0);
    // Pixels from the top-left to Vulkan NDC (y points down).
    gl_Position = vec4(p.xy * pc.to_ndc - 1.0, 0.0, 1.0);
    color = in_color;
    uv = in_uv;
}
