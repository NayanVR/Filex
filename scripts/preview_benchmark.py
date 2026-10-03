#!/usr/bin/env python3
"""Isolated Windows comparison runner. Standard library only; no private files.
Each native operation has an external deadline. Preserve raw failures and partial
checkpoints, publish the report even when the runner cannot render a window.
"""
import argparse
from collections import Counter
import ctypes
import html
import hashlib
import json
import math
import os
from pathlib import Path
import random
import shutil
import statistics
import subprocess
import sys
import threading
import time
import wave
import zipfile


def percentile(values, percentile):
    """Nearest-rank percentile, including the maximum for small p99 samples."""
    values = sorted(values)
    return values[max(0, math.ceil(len(values) * percentile / 100) - 1)] if values else None


def stats(values):
    values = [float(v) for v in values if v is not None and math.isfinite(v)]
    return {"n": len(values), "p50": percentile(values, 50), "p95": percentile(values, 95),
            "p99": percentile(values, 99), "mean": statistics.mean(values) if values else None}


def write(path, value):
    path.write_text(json.dumps(value, indent=2), encoding="utf-8")


class Sampler:
    """Sample child private commit/RSS every 10 ms. Kernel peak WS is also read.
    COM surrogates are not children: their memory is reported separately by the
    native preview host when it discovers a handler HWND/PID.
    """
    def __init__(self, process):
        self.process = process
        self.stop = threading.Event()
        self.samples = []
        self.error = None
        self.thread = threading.Thread(target=self.run, daemon=True)

    def run(self):
        if os.name != "nt":
            self.error = "Windows process counters unavailable"
            return
        from ctypes import wintypes as w
        class Counters(ctypes.Structure):
            _fields_ = [("cb", w.DWORD), ("PageFaultCount", w.DWORD)] + [
                (name, ctypes.c_size_t) for name in ["PeakWorkingSetSize", "WorkingSetSize",
                "QuotaPeakPagedPoolUsage", "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
                "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage", "PrivateUsage"]]
        query = ctypes.WinDLL("psapi", use_last_error=True).GetProcessMemoryInfo
        query.argtypes = [w.HANDLE, ctypes.POINTER(Counters), w.DWORD]
        query.restype = w.BOOL
        start = time.perf_counter()
        while not self.stop.is_set():
            counters = Counters(); counters.cb = ctypes.sizeof(counters)
            if query(int(self.process._handle), ctypes.byref(counters), counters.cb):
                self.samples.append({"elapsed_ms": (time.perf_counter()-start)*1000,
                    "private_bytes": counters.PrivateUsage, "working_set_bytes": counters.WorkingSetSize,
                    "kernel_peak_working_set_bytes": counters.PeakWorkingSetSize})
            else:
                self.error = f"GetProcessMemoryInfo: {ctypes.get_last_error()}"
            self.stop.wait(.01)

    def finish(self):
        self.stop.set(); self.thread.join(timeout=2)
        return {"interval_ms": 10, "scope": "worker process only, excludes COM surrogates and GPU memory",
                "error": self.error, "samples": self.samples,
                "peak_sampled_private_bytes": max((v["private_bytes"] for v in self.samples), default=None),
                "peak_working_set_bytes": max((v["kernel_peak_working_set_bytes"] for v in self.samples), default=None)}


def case(executable, arguments, output, timeout):
    """Timeout/error never masquerades as a successful checkpoint."""
    started = time.perf_counter()
    with output.with_suffix(".log").open("wb") as log:
        process = subprocess.Popen([str(executable), *map(str, arguments), str(output)], stdout=log, stderr=log)
        sampler = Sampler(process); sampler.thread.start()
        timed_out = False
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            if os.name == "nt":
                subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], stdout=log, stderr=log, check=False)
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)
        counters = sampler.finish()
    try:
        result = json.loads(output.read_text(encoding="utf-8"))
    except (ValueError, OSError):
        result = {"status": "missing_result"}
    if timed_out:
        result["status"] = "timeout"
    elif process.returncode:
        result["status"] = "process_failed"
    result.update({"exit_code": process.returncode, "wall_seconds": time.perf_counter()-started,
                   "process_memory": counters, "arguments": list(map(str, arguments))})
    write(output, result)
    return result


def document_fixtures(directory):
    directory.joinpath("text.txt").write_text("Filex preview benchmark\n" * 200, encoding="utf-8")
    directory.joinpath("text.rtf").write_text(r"{\rtf1\ansi\b Filex preview benchmark\b0\par Rich text preview.}", encoding="ascii")
    directory.joinpath("page.html").write_text("<!doctype html><title>Filex</title><h1>Preview benchmark</h1><p>Generated local fixture.</p>", encoding="utf-8")
    directory.joinpath("drawing.svg").write_text('<svg xmlns="http://www.w3.org/2000/svg" width="600" height="400"><rect width="600" height="400" fill="#40a0e0"/><circle cx="300" cy="200" r="100" fill="#ffcc00"/></svg>', encoding="ascii")
    # Valid PDF with a xref table, no external font or URL dependency.
    objects = [b'<< /Type /Catalog /Pages 2 0 R >>', b'<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
               b'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>',
               b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>']
    stream = b'BT /F1 28 Tf 50 700 Td (Filex preview benchmark) Tj ET\n'
    objects.append(b'<< /Length '+str(len(stream)).encode()+b' >>\nstream\n'+stream+b'endstream')
    pdf = bytearray(b'%PDF-1.4\n'); offsets = [0]
    for i, obj in enumerate(objects, 1):
        offsets.append(len(pdf)); pdf.extend(f'{i} 0 obj\n'.encode()+obj+b'\nendobj\n')
    xref = len(pdf); pdf.extend(f'xref\n0 {len(offsets)}\n0000000000 65535 f \n'.encode())
    for offset in offsets[1:]: pdf.extend(f'{offset:010d} 00000 n \n'.encode())
    pdf.extend(f'trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n'.encode())
    directory.joinpath("document.pdf").write_bytes(pdf)
    ns = 'http://schemas.openxmlformats.org/'
    with zipfile.ZipFile(directory/'document.docx', 'w', zipfile.ZIP_DEFLATED) as z:
        z.writestr('[Content_Types].xml', '<Types xmlns="'+ns+'package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
        z.writestr('_rels/.rels', '<Relationships xmlns="'+ns+'package/2006/relationships"><Relationship Id="rId1" Type="'+ns+'officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
        z.writestr('word/document.xml', '<w:document xmlns:w="'+ns+'wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Filex preview benchmark</w:t></w:r></w:p></w:body></w:document>')
    with zipfile.ZipFile(directory/'archive.zip', 'w') as z:
        z.writestr('hello.txt', 'Generated archive fixture')
    with wave.open(str(directory/'audio.wav'), 'wb') as audio:
        audio.setparams((1, 2, 16000, 0, 'NONE', 'not compressed'))
        import struct
        audio.writeframes(b''.join(struct.pack('<h', int(10000*math.sin(i*2*math.pi*440/16000))) for i in range(32000)))
    ffmpeg = shutil.which('ffmpeg')
    if not ffmpeg:
        return {"video": "unavailable: ffmpeg not installed"}
    command = [ffmpeg, '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', 'testsrc=size=640x360:rate=24',
               '-t', '2', '-c:v', 'libx264', '-pix_fmt', 'yuv420p', '-y', str(directory/'video.mp4')]
    result = subprocess.run(command, capture_output=True, timeout=60)
    return {"video": "ok" if result.returncode == 0 else result.stderr.decode(errors='replace')}


def summarize(result, kind):
    if kind == 'decode':
        return {"latency_ms": stats(result.get('samples_ms', [])), "failures": len(result.get('errors', []))}
    if kind == 'scroll':
        phases = {}
        for phase in ['slow', 'fast', 'revisit']:
            frames = [v for v in result.get('frames', []) if v['phase'] == phase]
            intervals = [v['frame_interval_ms'] for v in frames if v['frame_interval_ms'] is not None]
            phases[phase] = {"interval_ms": stats(intervals),
                             "over_16_67_ms": sum(v > 16.67 for v in intervals),
                             "over_33_33_ms": sum(v > 33.33 for v in intervals),
                             "render_to_callback_ms": stats([v['render_to_callback_ms'] for v in frames])}
        return {"phases": phases, "load_ms": stats([v['request_to_ready_ms'] for v in result.get('loads', []) if v['ok']]),
                "load_failures": sum(not v['ok'] for v in result.get('loads', [])), "peak_in_flight": result.get('peak_in_flight')}
    return {"cycles": len(result.get('cycles', [])),
            "open_api_ms": stats([v['open_api_ms'] for v in result.get('cycles', [])]),
            "child_window_ms": stats([v['child_window_ms'] for v in result.get('cycles', [])]),
            "resize_api_ms": stats([v['resize_api_ms'] for v in result.get('cycles', [])]),
            "unload_ms": stats([v['unload_and_release_ms'] for v in result.get('cycles', [])])}


def failure_details(result):
    """Aggregate repeated failures without hiding the native error code."""
    errors=Counter(v.get('error', 'unknown load failure') for v in result.get('loads', []) if not v.get('ok'))
    if result.get('error'):
        errors[result['error']]+=1
    return [{'error':message, 'count':count} for message,count in errors.most_common()]


def required_case(record):
    return record['kind']=='scroll' or (record['kind']=='decode' and record.get('fixture')=='landscape.jpg' and record.get('mode')!='cache')


def failure_reason(record):
    errors=record.get('failure_details',[])
    detail='; '.join(f"{v['count']} × {v['error']}" for v in errors[:3])
    return f"{record['result']}: {record['status']}" + (f" — {detail}" if detail else '')


def memory_summary(result):
    before=result.get('memory_before', {})
    after=result.get('memory_after_cleanup', result.get('memory_after', {}))
    def delta(key):
        a,b=before.get(key),after.get(key)
        return b-a if isinstance(a,(int,float)) and isinstance(b,(int,float)) else None
    handler_rss=[v.get('handler_memory',{}).get('working_set_bytes')
                 for v in result.get('cycles',[]) if isinstance(v.get('handler_memory'),dict)]
    return {'peak_sampled_private_bytes':result.get('process_memory',{}).get('peak_sampled_private_bytes'),
            'private_bytes_after_minus_before':delta('private_bytes'), 'cpu_ms':delta('cpu_ms'),
            'gdi_handle_delta':delta('gdi_handles'), 'user_handle_delta':delta('user_handles'),
            'max_handler_working_set_bytes':max((v for v in handler_rss if v is not None),default=None)}


def number(value):
    if not isinstance(value, (int, float)):
        return '—'
    return f'{value:.6f}' if 0 < abs(value) < .001 else f'{value:.3f}'


def report(output, records, fixtures):
    lines = ['# Windows preview and thumbnail comparison', '',
             'This run tests real APIs and a live GPUI component window. Missing providers, crashes and timeouts remain visible.', '',
             '**Scope:** synthetic files; first-path requests do not flush OS disk caches. Shell requests forbid icon fallback. '
             'Repeat requests may hit Shell caches; Filex repeat requests decode again. Cache-only compares Filex RAM lookup with Shell cache retrieval, different layers.', '',
             '**Memory:** per-worker private commit and working set, sampled every 10 ms, plus OS peak working set. '
             'Preview-handler PID counters are in raw cycle records when accessible. Shell surrogate/GPU memory is not included in worker totals. '
             'Peak and post-cleanup growth alone do not prove a leak.', '',
             '**UI:** actual production GPUI card/details components and cache, two background jobs, 1,200 files. '
             'This excludes Workspace chrome/indexing and uses programmatic scrolling. Frame callback intervals are not physical display FPS. '
             'Three rounds alternate backend order; software/virtual graphics on a hosted server may dominate.', '',
             '**Full previews:** native IPreviewHandler open/resize/unload runs in a real HWND with a message pump. '
             'API completion/child HWND appearance are not first-paint latency. Review screenshots. '
             '**Filex has no Windows full-file viewer; its existing thumbnail/details pane is exercised in the UI test.**', '',
             '## Thumbnails', '', '| Fixture | Backend | Mode | Status | Successes | p50 ms | p95 ms | Worker peak WS MiB |',
             '|---|---|---|---|---:|---:|---:|---:|']
    failed=[r for r in records if (required_case(r) and r['status']!='ok') or r['status'] in ['timeout','process_failed','missing_result','handler_error']]
    if failed:
        lines[2:2]=['## Cases needing attention', '', *['- '+failure_reason(r) for r in failed], '', 'Scrolling cases without successful thumbnail loads are invalid for a thumbnail performance comparison.', '']
    for record in records:
        if record['kind'] != 'decode': continue
        s=record['summary']['latency_ms']; peak=record.get('peak_ws')
        lines.append(f"| {record['fixture']} | {record['backend']} | {record['mode']} | {record['status']} | {s['n']} | {number(s['p50'])} | {number(s['p95'])} | {number(peak/1048576 if peak else None)} |")
    lines += ['', '## Live scrolling and details pane', '', '| Backend / round | Status | Phase | Frames | p50 interval ms | p95 interval ms | >33.33 ms |', '|---|---|---|---:|---:|---:|---:|']
    for record in records:
        if record['kind'] != 'scroll': continue
        phases=record['summary']['phases']
        for phase,s in phases.items():
            lines.append(f"| {record['backend']} / {record['round']} | {record['status']} | {phase} | {s['interval_ms']['n']} | {number(s['interval_ms']['p50'])} | {number(s['interval_ms']['p95'])} | {s['over_33_33_ms']} |")
    lines += ['', '## Full preview handlers', '', '| Fixture | Status | Cycles | p50 open API ms | p50 child-window ms |', '|---|---|---:|---:|---:|']
    for record in records:
        if record['kind'] != 'preview': continue
        s=record['summary']
        lines.append(f"| {record['fixture']} | {record['status']} | {s['cycles']} | {number(s['open_api_ms']['p50'])} | {number(s['child_window_ms']['p50'])} |")
    lines += ['', '## Memory, CPU and cleanup', '',
              '| Case | Peak sampled private MiB | Retained private delta MiB | CPU ms | GDI / USER delta | External preview handler max observed WS MiB |',
              '|---|---:|---:|---:|---|---:|']
    for record in records:
        if record['kind']=='decode' and record['mode']!='repeat': continue
        m=record['memory']
        mib=lambda value: number(value/1048576 if value is not None else None)
        lines.append(f"| {record['result'][:-5]} | {mib(m['peak_sampled_private_bytes'])} | {mib(m['private_bytes_after_minus_before'])} | {number(m['cpu_ms'])} | {m['gdi_handle_delta']} / {m['user_handle_delta']} | {mib(m['max_handler_working_set_bytes'])} |")
    lines += ['', '## Coverage and evidence' , '', f'Video fixture: {fixtures.get("video")}', '',
              'Read `results.json` and per-case JSON/logs for errors, raw timings, memory samples, CPU counters and lifecycle details. '
              'Open `gallery.html` for thumbnail and preview screenshots. No unsupported format is counted as a successful preview.']
    output.joinpath('summary.md').write_text('\n'.join(lines)+'\n', encoding='utf-8')
    images=['<!doctype html><meta charset="utf-8"><title>Filex benchmark evidence</title><style>body{font:16px system-ui;background:#eee}figure{display:inline-block;vertical-align:top;width:360px;overflow-wrap:anywhere}img{max-width:350px;max-height:300px;background:repeating-conic-gradient(#ddd 0% 25%,white 0% 50%) 0/16px 16px}</style><h1>Thumbnail and preview evidence</h1><p>Screenshots are evidence for visual review, not an automated correctness verdict.</p>']
    for path in sorted(output.glob('*.png')):
        images.append(f'<figure><img src="{html.escape(path.name, quote=True)}"><figcaption>{html.escape(path.stem)}</figcaption></figure>')
    output.joinpath('gallery.html').write_text('\n'.join(images), encoding='utf-8')
    write(output/'results.json', {'fixtures': fixtures, 'records': records})


def main():
    parser=argparse.ArgumentParser(); parser.add_argument('--exe', type=Path, required=True); parser.add_argument('--output', type=Path, required=True)
    args=parser.parse_args(); output=args.output.resolve(); output.mkdir(parents=True, exist_ok=True)
    executable=args.exe.resolve(); corpus=output/'fixtures'; corpus.mkdir(exist_ok=True)
    subprocess.run([str(executable), 'fixtures', str(corpus)], check=True, timeout=180)
    fixtures=document_fixtures(corpus); write(output/'fixture-status.json',fixtures)
    paths=sorted(corpus.iterdir()); records=[]
    manifest=[]
    for p in paths:
        with p.open('rb') as source:
            manifest.append({'name':p.name,'bytes':p.stat().st_size,'sha256':hashlib.file_digest(source,'sha256').hexdigest()})
    write(output/'fixture-manifest.json',manifest)
    decode_cases=[(path,backend,mode) for path in paths for backend in ['filex','shell'] for mode in ['first','repeat','cache']]
    random.Random(20261002).shuffle(decode_cases)
    def execute(kind, key, arguments, metadata, timeout=90):
        print(key, flush=True)
        result=case(executable, arguments, output/f'{key}.json', timeout)
        record={'kind':kind,'result':f'{key}.json','status':result['status'],**metadata,
                'failure_details':failure_details(result),'summary':summarize(result,kind),'gpu':result.get('gpu'),'memory':memory_summary(result),'peak_ws':result['process_memory']['peak_working_set_bytes']}
        records.append(record)
        report(output,records,fixtures)  # incremental report survives later runner cancellation
    for path,backend,mode in decode_cases:
        execute('decode',f'thumb-{path.name}-{backend}-{mode}', ['decode',backend,path,mode], {'fixture':path.name,'backend':backend,'mode':mode})
    # Real paths with a >512-entry working set; copies avoid Shell treating hardlinks as one identity.
    scroll=output/'scroll-corpus'; scroll.mkdir(exist_ok=True)
    for i in range(1200):
        source=corpus/(['landscape.jpg','landscape.png','portrait.jpg','alpha.png'][i%4])
        shutil.copyfile(source,scroll/f'{i:04d}-Background Verification Form{source.suffix}')
    for round_index,order in enumerate([['icons','filex','shell'],['shell','icons','filex'],['filex','shell','icons']],1):
        for backend in order:
            execute('scroll',f'scroll-{backend}-{round_index}', ['scroll',backend,scroll], {'backend':backend,'round':round_index},60)
    for path in paths:
        execute('preview',f'preview-{path.name}', ['preview',path], {'fixture':path.name},45)
    failures=[r for r in records if r['status'] in ['timeout','process_failed','missing_result','running'] or (r['status']=='handler_error' and r.get('fixture') not in ['corrupt.png','size-limit.png','dimension-limit.png'])]
    required=[r for r in records if required_case(r)]
    missing=[r for r in required if r['status']!='ok']
    write(output/'coverage.json',{'completed_cases':len(records),'process_failures':len(failures),'required_case_failures':len(missing),'required_failure_details':[failure_reason(r) for r in missing],
        'filex_windows_full_preview':'not implemented','video_fixture':fixtures['video'],
        'full_application_scrolling':'not measured; production grid/details component workload measured',
        'native_preview_first_paint':'not measured; API completion and child window creation measured',
        'native_preview_cases_ok':sum(r['kind']=='preview' and r['status']=='ok' for r in records),
        'unsupported_preview_formats':[r['fixture'] for r in records if r['kind']=='preview' and r['status']=='unsupported']})
    if not any(r['kind']=='preview' and r['status']=='ok' for r in records):
        print('::warning::No installed preview handler completed every lifecycle check; inspect provider inventory and per-format results.')
    if failures or missing or fixtures['video']!='ok':
        for record in {r['result']: r for r in failures+missing}.values():
            print(failure_reason(record),flush=True)
        print('::error::Benchmark coverage is incomplete or a worker failed; download the report and logs.', flush=True)
        return 1
    return 0

if __name__=='__main__':
    sys.exit(main())
