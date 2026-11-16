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
uniform float tf_id;
uniform mat3 primary_matrix;
uniform vec2 luminance;
uniform float white_nits;
uniform float hdr_output;
varying vec2 v_coords;
#ifdef DEBUG_FLAGS
uniform float tint;
#endif
vec3 decode(vec3 c) {
    if (tf_id == 5.0) return c;
    if (tf_id == 2.0) return sign(c)*pow(abs(c),vec3(2.2));
    if (tf_id == 11.0) {
        vec3 p=pow(clamp(c,0.0,1.0),vec3(32.0/2523.0));
        return pow(max(p-3424.0/4096.0,0.0)/(2413.0/128.0-2392.0/128.0*p),vec3(16384.0/2610.0));
    }
    vec3 a=abs(c);
    return sign(c)*mix(a/12.92,pow((a+0.055)/1.055,vec3(2.4)),step(vec3(0.04045),a));
}
vec3 encode(vec3 c) {
    vec3 a=abs(c);
    return sign(c)*mix(a*12.92,1.055*pow(a,vec3(1.0/2.4))-0.055,step(vec3(0.0031308),a));
}
void main() {
    vec4 sample_color=texture2D(tex,v_coords);
    vec3 c=sample_color.a > 0.000001 ? sample_color.rgb/sample_color.a : vec3(0.0);
    c=primary_matrix*(decode(c)*luminance.x+vec3(luminance.y))/white_nits;
    if (hdr_output < 0.5) {
        // Hue-preserving shoulder above 75% of SDR peak, asymptotic to white.
        c=max(c,0.0);
        float peak=max(c.r,max(c.g,c.b));
        if (peak > 0.75) c *= (0.75+0.25*(1.0-exp(-(peak-0.75)/0.25)))/peak;
    }
    c=encode(c);
#ifdef DEBUG_FLAGS
    if (tint == 1.0) c=mix(c,vec3(0.0,1.0,0.0),0.2);
#endif
    gl_FragColor=vec4(c*sample_color.a,sample_color.a)*alpha;
}
