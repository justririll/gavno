#version 100
//_DEFINES_
#ifdef EXTERNAL
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;
#ifdef EXTERNAL
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
uniform float white_nits;
uniform float extended_input;
// sRGB to BT.2020 with the night-light white point folded in.
uniform mat3 to2020;
varying vec2 v_coords;
#ifdef DEBUG_FLAGS
uniform float tint;
#endif
// COLOR_CONSTANTS
vec3 srgb_linear(vec3 c) {
    vec3 a = abs(c);
    return sign(c) * mix(a / srgb_slope, pow((a + srgb_offset) / srgb_scale, vec3(srgb_exponent)), step(vec3(srgb_break), a));
}
vec3 pq(vec3 nits) {
    vec3 p = pow(clamp(nits / 10000.0, 0.0, 1.0), vec3(pq_m1));
    return pow((pq_c1 + pq_c2 * p) /
               (1.0 + pq_c3 * p), vec3(pq_m2));
}
void main() {
    // SDR composition is opaque, including the NO_ALPHA shader variant.
    vec3 color = texture2D(tex, v_coords).rgb;
    if (extended_input < 0.5) color = clamp(color, 0.0, 1.0);
#ifdef DEBUG_FLAGS
    if (tint == 1.0) color = mix(color, vec3(0.0, 1.0, 0.0), 0.2);
#endif
    vec3 c = srgb_linear(color);
    gl_FragColor = vec4(pq(to2020 * c * white_nits), 1.0) * alpha;
}
