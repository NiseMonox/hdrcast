import { html, useEffect, useLayoutEffect, useRef, useState } from './vendor/preact-htm.js';
import { Icon } from './icons.js';

export function Switch({ on, onChange, disabled }) {
  return html`<button type="button" role="switch" aria-checked=${on ? 'true' : 'false'} disabled=${disabled}
    class=${'switch' + (on ? ' on' : '')} onClick=${() => onChange(!on)}></button>`;
}

/** options: [{value, label}] */
export function Segmented({ value, options, onChange, disabled }) {
  const ref = useRef(null);
  const [thumb, setThumb] = useState(null);
  const index = Math.max(0, options.findIndex((o) => o.value === value));
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () => {
      const b = el.querySelectorAll('button')[index];
      if (b) setThumb({ x: b.offsetLeft - 2, w: b.offsetWidth });
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [index, options.length, options.map((o) => o.label).join('|')]);
  return html`<div class="segmented" ref=${ref} role="radiogroup">
    ${thumb && html`<div class="thumb" style=${`width:${thumb.w}px;transform:translateX(${thumb.x}px)`}></div>`}
    ${options.map((o) => html`<button type="button" role="radio" aria-checked=${o.value === value ? 'true' : 'false'}
      disabled=${disabled} onClick=${() => onChange(o.value)}>${o.label}</button>`)}
  </div>`;
}

/** options: [{value, label}] — values are compared as strings and handed back as given. */
export function Select({ value, options, onChange, disabled, style }) {
  return html`<span class="select" style=${style}><select disabled=${disabled} value=${String(value)}
    onChange=${(e) => {
      const hit = options.find((o) => String(o.value) === e.target.value);
      if (hit) onChange(hit.value);
    }}>
    ${options.map((o) => html`<option value=${String(o.value)} selected=${String(o.value) === String(value)}>${o.label}</option>`)}
  </select></span>`;
}

export function Slider({ value, min, max, step, onChange, disabled }) {
  const p = ((value - min) / (max - min)) * 100;
  return html`<input type="range" class="slider" min=${min} max=${max} step=${step || 1} value=${value}
    disabled=${disabled} style=${`--p:${Math.max(0, Math.min(100, p))}%`} onInput=${(e) => onChange(Number(e.target.value))} />`;
}

/** A number box that commits on blur / Enter and snaps back when the text is not a number in range. */
export function NumberField({ value, min, max, step, onChange, disabled, width }) {
  const [text, setText] = useState(String(value));
  useEffect(() => setText(String(value)), [value]);
  const commit = () => {
    const n = Number(text);
    if (text.trim() === '' || !Number.isFinite(n)) {
      setText(String(value));
      return;
    }
    const decimals = step && step < 1 ? 1 : 0;
    const clamped = Number(Math.min(max, Math.max(min, n)).toFixed(decimals));
    setText(String(clamped));
    if (clamped !== value) onChange(clamped);
  };
  return html`<input class="field num" inputmode="decimal" disabled=${disabled} value=${text} style=${width ? `width:${width}px` : ''}
    onInput=${(e) => setText(e.target.value)} onBlur=${commit}
    onKeyDown=${(e) => { if (e.key === 'Enter') e.target.blur(); }} />`;
}

export function TextField({ value, onChange, placeholder, disabled, className, maxLength }) {
  const [text, setText] = useState(value || '');
  useEffect(() => setText(value || ''), [value]);
  return html`<input class=${'field ' + (className || '')} disabled=${disabled} value=${text} placeholder=${placeholder || ''}
    maxlength=${maxLength} spellcheck=${false}
    onInput=${(e) => setText(e.target.value)} onBlur=${() => { if (text !== (value || '')) onChange(text.trim()); }}
    onKeyDown=${(e) => { if (e.key === 'Enter') e.target.blur(); }} />`;
}

export function Stepper({ value, min, max, onChange, disabled }) {
  return html`<span class="stepper">
    <button type="button" disabled=${disabled || value <= min} onClick=${() => onChange(value - 1)}><${Icon} name="minus" /></button>
    <span class="n">${value}</span>
    <button type="button" disabled=${disabled || value >= max} onClick=${() => onChange(value + 1)}><${Icon} name="plus" /></button>
  </span>`;
}

export function Row({ name, hint, children, disabled, wide, tile }) {
  return html`<div class=${'row' + (disabled ? ' disabled' : '')}>
    ${tile && html`<span class="icon-tile"><${Icon} name=${tile} /></span>`}
    <div class="label"><div class="name">${name}</div>${hint && html`<div class="hint">${hint}</div>`}</div>
    <div class=${'control' + (wide ? ' wide' : '')}>${children}</div>
  </div>`;
}

export function Section({ title, note, noteWarn, action, children }) {
  return html`
    ${title && html`<div class="section-title"><span>${title}</span><span class="spacer"></span>${action}</div>`}
    <div class="card rows">${children}</div>
    ${note && html`<div class=${'section-note' + (noteWarn ? ' warn' : '')}>${note}</div>`}`;
}

export function Disclosure({ label, open, onToggle }) {
  return html`<button type="button" class=${'disclosure' + (open ? ' open' : '')} onClick=${onToggle}>
    <${Icon} name="chevron" /><span>${label}</span></button>`;
}

// Greys only: people are told apart by their initial and name, not by colour.
const avatarColors = ['#48484a', '#636366', '#7c7c80', '#8e8e93', '#56565a', '#6e6e73'];

export function Avatar({ name, size }) {
  const text = (name || '?').trim();
  let hash = 0;
  for (const ch of text) hash = (hash * 31 + ch.codePointAt(0)) >>> 0;
  const first = Array.from(text)[0] || '?';
  return html`<span class=${'avatar ' + (size || '')} style=${`background:${avatarColors[hash % avatarColors.length]}`}>${first}</span>`;
}

export function Sheet({ children, wide, onClose }) {
  useEffect(() => {
    const onKey = (e) => { if (e.key === 'Escape' && onClose) onClose(); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);
  return html`<div class="overlay" onMouseDown=${(e) => { if (e.target === e.currentTarget && onClose) onClose(); }}>
    <div class=${'sheet' + (wide ? ' wide' : '')} role="dialog" aria-modal="true">${children}</div>
  </div>`;
}

export function Empty({ icon, title, text, children }) {
  return html`<div class="empty">
    <div class="glyph"><${Icon} name=${icon} /></div>
    <div class="etitle">${title}</div>
    ${text && html`<div class="etext">${text}</div>`}
    ${children && html`<div class="eactions">${children}</div>`}
  </div>`;
}

export function formatDuration(totalSeconds) {
  const s = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const pad = (n) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s % 60)}` : `${pad(m)}:${pad(s % 60)}`;
}
