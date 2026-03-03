// SVGTracker: intercepts Canvas2D calls and builds SVG markup.
// Shared between index.html (generateSVG) and headless.html (test harness).

export class SVGTracker {
    constructor(w, h) {
        this.w = w; this.h = h;
        this.elements = [];
        this.defs = [];
        this.path = "";
        this.state = {};
        this.stack = [];
        this.pendingImages = [];
    }

    _r(v) { return Math.round(v); }

    _parseColor(color) {
        if (!color) return { c: "none", o: 1 };
        if (typeof color === 'string' && color.startsWith('rgba')) {
            const m = color.match(/rgba\(([^,]+),([^,]+),([^,]+),([^)]+)\)/);
            if (m) return { c: `rgb(${m[1]},${m[2]},${m[3]})`, o: parseFloat(m[4]) };
        }
        return { c: color, o: 1 };
    }

    _style(type, usePathTransform) {
        const c = type === 'fill' ? this.state.fillStyle : this.state.strokeStyle;
        const pc = this._parseColor(c);
        const opacity = pc.o * this.state.globalAlpha;
        let s = opacity < 1 ? `opacity="${+opacity.toFixed(3)}"` : '';

        const t = usePathTransform ? this._pathTransform : this.state.transform;
        if (t && t.join(',') !== '1,0,0,1,0,0') {
            s += ` transform="matrix(${t.join(' ')})"`;
        }
        const gco = this.state.globalCompositeOperation;
        if (gco && gco !== 'source-over') {
            s += ` style="mix-blend-mode: ${gco};"`;
        }

        if (type === 'fill') {
            s += ` fill="${pc.c}"`;
        } else {
            s += ` fill="none" stroke="${pc.c}" stroke-width="${this.state.lineWidth}" stroke-linecap="${this.state.lineCap}" stroke-linejoin="${this.state.lineJoin}"`;
            if (this.state.lineDash && this.state.lineDash.length > 0) s += ` stroke-dasharray="${this.state.lineDash.join(',')}"`;
        }
        return s;
    }

    save() { this.stack.push(JSON.parse(JSON.stringify(this.state))); this.elements.push('<g>'); }
    restore() { if (this.stack.length > 0) this.state = this.stack.pop(); this.elements.push('</g>'); }
    setTransform(a,b,c,d,e,f) { this.state.transform = [a,b,c,d,e,f]; }
    clearRect(x,y,w,h) {}
    fillRect(x,y,w,h) { this.elements.push(`<rect x="${this._r(x)}" y="${this._r(y)}" width="${this._r(w)}" height="${this._r(h)}" ${this._style('fill')} />`); }
    strokeRect(x,y,w,h) { this.elements.push(`<rect x="${this._r(x)}" y="${this._r(y)}" width="${this._r(w)}" height="${this._r(h)}" ${this._style('stroke')} />`); }
    beginPath() { this.path = ""; this._pathTransform = this.state.transform ? [...this.state.transform] : null; }
    moveTo(x,y) { this.path += `M ${this._r(x)} ${this._r(y)} `; }
    lineTo(x,y) { this.path += `L ${this._r(x)} ${this._r(y)} `; }
    quadraticCurveTo(cx,cy,x,y) { this.path += `Q ${this._r(cx)} ${this._r(cy)} ${this._r(x)} ${this._r(y)} `; }
    bezierCurveTo(c1x,c1y,c2x,c2y,x,y) { this.path += `C ${this._r(c1x)} ${this._r(c1y)} ${this._r(c2x)} ${this._r(c2y)} ${this._r(x)} ${this._r(y)} `; }
    closePath() { if (this.path !== "") this.path += "Z "; }

    ellipse(cx, cy, rx, ry, rot, startAngle, endAngle) {
        if (rx === 0 || ry === 0) return;
        if (Math.abs(endAngle - startAngle) >= Math.PI * 1.99) {
            const x1 = cx - Math.cos(rot)*rx; const y1 = cy - Math.sin(rot)*rx;
            const x2 = cx + Math.cos(rot)*rx; const y2 = cy + Math.sin(rot)*rx;
            this.path += `M ${this._r(x1)} ${this._r(y1)} A ${this._r(rx)} ${this._r(ry)} ${this._r(rot * 180 / Math.PI)} 1 0 ${this._r(x2)} ${this._r(y2)} A ${this._r(rx)} ${this._r(ry)} ${this._r(rot * 180 / Math.PI)} 1 0 ${this._r(x1)} ${this._r(y1)} `;
        }
    }

    arc(x, y, r, sa, ea, anticlockwise) {
        if (r === 0) return;
        if (Math.abs(ea - sa) >= Math.PI * 1.99) {
            this.path += `M ${this._r(x-r)} ${this._r(y)} A ${this._r(r)} ${this._r(r)} 0 1 0 ${this._r(x+r)} ${this._r(y)} A ${this._r(r)} ${this._r(r)} 0 1 0 ${this._r(x-r)} ${this._r(y)} `;
        } else {
            const sx = x + r * Math.cos(sa); const sy = y + r * Math.sin(sa);
            const ex = x + r * Math.cos(ea); const ey = y + r * Math.sin(ea);
            let drawn = anticlockwise ? sa - ea : ea - sa;
            while (drawn < 0) drawn += 2 * Math.PI;
            while (drawn >= 2 * Math.PI) drawn -= 2 * Math.PI;
            const largeArc = drawn > Math.PI ? 1 : 0;
            const sweep = anticlockwise ? 0 : 1;
            if (this.path === "") this.path += `M ${this._r(sx)} ${this._r(sy)} `; else this.path += `L ${this._r(sx)} ${this._r(sy)} `;
            this.path += `A ${this._r(r)} ${this._r(r)} 0 ${largeArc} ${sweep} ${this._r(ex)} ${this._r(ey)} `;
        }
    }

    rect(x,y,w,h) { this.path += `M ${this._r(x)} ${this._r(y)} L ${this._r(x+w)} ${this._r(y)} L ${this._r(x+w)} ${this._r(y+h)} L ${this._r(x)} ${this._r(y+h)} Z `; }
    fill(p) { const d = p && p.__svgStr ? p.__svgStr : this.path; if(d) this.elements.push(`<path d="${d}" ${this._style('fill', true)} />`); }
    stroke(p) { const d = p && p.__svgStr ? p.__svgStr : this.path; if(d) this.elements.push(`<path d="${d}" ${this._style('stroke', true)} />`); }

    fillText(text, x, y) {
        let dy = "0";
        if (this.state.textBaseline === "top") dy = "0.8em";
        else if (this.state.textBaseline === "middle") dy = "0.3em";
        else if (this.state.textBaseline === "bottom") dy = "-0.2em";
        let align = "start";
        if (this.state.textAlign === "center") align = "middle";
        if (this.state.textAlign === "right") align = "end";
        this.elements.push(`<text x="${this._r(x)}" y="${this._r(y)}" dy="${dy}" style="font: ${this.state.font};" text-anchor="${align}" ${this._style('fill')}>${text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")}</text>`);
    }

    setLineDash(d) { this.state.lineDash = d ? Array.from(d) : []; }

    drawImage(img, x, y, w, h) {
        const placeholder = `__IMG_${this.pendingImages.length}__`;
        this.pendingImages.push({ src: img.src, naturalWidth: img.naturalWidth || img.width, naturalHeight: img.naturalHeight || img.height });
        this.elements.push(`<image href="${placeholder}" x="${this._r(x)}" y="${this._r(y)}" width="${this._r(w)}" height="${this._r(h)}" opacity="${this.state.globalAlpha}" transform="matrix(${this.state.transform.join(' ')})" preserveAspectRatio="none" />`);
    }

    createPattern(canvas, rep) {
        const id = "pattern_" + Math.random().toString(36).substr(2, 9);
        this.defs.push(`<pattern id="${id}" width="${canvas.width}" height="${canvas.height}" patternUnits="userSpaceOnUse"><image href="${canvas.toDataURL()}" width="${canvas.width}" height="${canvas.height}" /></pattern>`);
        return { __svgId: id };
    }

    async getSVG() {
        let body = this.elements.join('\n');
        for (let i = 0; i < this.pendingImages.length; i++) {
            const pi = this.pendingImages[i];
            let dataUri = pi.src;
            if (!dataUri.startsWith('data:')) {
                try {
                    const resp = await fetch(pi.src);
                    const blob = await resp.blob();
                    const mime = blob.type || 'image/png';
                    if (mime === 'image/svg+xml') {
                        const text = await blob.text();
                        dataUri = 'data:image/svg+xml;base64,' + btoa(unescape(encodeURIComponent(text)));
                    } else {
                        dataUri = await new Promise(r => { const fr = new FileReader(); fr.onload = () => r(fr.result); fr.readAsDataURL(blob); });
                    }
                } catch(e) {
                    try {
                        const c = document.createElement('canvas');
                        c.width = pi.naturalWidth; c.height = pi.naturalHeight;
                        const img = new Image(); img.crossOrigin = 'anonymous'; img.src = pi.src;
                        await new Promise((res, rej) => { img.onload = res; img.onerror = rej; });
                        c.getContext('2d').drawImage(img, 0, 0);
                        dataUri = c.toDataURL('image/png');
                    } catch(e2) {}
                }
            }
            body = body.replace(`__IMG_${i}__`, dataUri);
        }
        return `<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="${this.w}" height="${this.h}" viewBox="0 0 ${this.w} ${this.h}">\n<defs>\n${this.defs.join('\n')}\n</defs>\n${body}\n</svg>`;
    }
}

// Create an SVG-tracking proxy around a real canvas context.
// colorFilter is optional: (prop, value) => value (for kaleido etc.)
export function createSVGProxy(canvas, tracker, colorFilter) {
    const realCtx = canvas.getContext('2d');

    if (!window.__origPath2D) {
        window.__origPath2D = window.Path2D;
        window.Path2D = function(pathStr) {
            const p = new window.__origPath2D(pathStr);
            p.__svgStr = pathStr;
            return p;
        };
    }

    const proxy = new Proxy(realCtx, {
        get(target, prop) {
            if (typeof target[prop] === 'function') {
                return function(...args) {
                    let svgRes;
                    if (typeof tracker[prop] === 'function') {
                        svgRes = tracker[prop].apply(tracker, args);
                    }
                    const res = target[prop].apply(target, args);
                    if (prop === 'createPattern' && svgRes) res.__svgId = svgRes.__svgId;
                    return res;
                };
            }
            if (prop in tracker.state) return tracker.state[prop];
            return target[prop];
        },
        set(target, prop, value) {
            if (prop in tracker.state) {
                if (prop === 'fillStyle' && value && value.__svgId) {
                    tracker.state[prop] = `url(#${value.__svgId})`;
                } else if (colorFilter && (prop === 'fillStyle' || prop === 'strokeStyle') && typeof value === 'string') {
                    tracker.state[prop] = colorFilter(value);
                } else tracker.state[prop] = value;
            }
            target[prop] = value;
            return true;
        }
    });

    const origGetContext = canvas.getContext;
    canvas.getContext = function(type, ...args) {
        if (type === '2d') return proxy;
        return origGetContext.call(this, type, ...args);
    };

    tracker.state = {
        fillStyle: "#000000", strokeStyle: "#000000", lineWidth: 1, lineCap: "butt",
        lineJoin: "miter", globalAlpha: 1, transform: [1,0,0,1,0,0],
        font: "10px sans-serif", textAlign: "start", textBaseline: "alphabetic",
        globalCompositeOperation: "source-over", lineDash: []
    };

    return proxy;
}
