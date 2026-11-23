// Browser APIs and asynchronous backpressure. MPEG-TS parsing lives in Rust.
const BLOCK = 188 * 4096;
const IN_FLIGHT = 5;

export class BrowserDecoder {
    constructor(demux) {
        this.demux = demux;
        this.events = [];
        this.epoch = 0;
        this.openedFile = false;
        this.input = document.createElement("input");
        this.input.type = "file";
        this.input.accept = ".ts,video/mp2t";
        this.input.hidden = true;
        document.body.append(this.input);
        this.input.addEventListener("change", () => {
            const file = this.input.files[0];
            if (file) void this.open(file);
        });
    }

    chooseFile() {
        this.input.value = "";
        this.input.click();
    }

    takeOpenedFile() {
        const opened = this.openedFile;
        this.openedFile = false;
        return opened;
    }

    close() {
        ++this.epoch;
        this.stopDecoder();
        this.keepBeforeTarget(null);
        this.input.remove();
    }

    stopDecoder() {
        if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
        this.decoder = null;
    }

    report(kind, message) {
        if (!this.events.some(event => event.kind === kind && event.message === message)) {
            this.events.push({ kind, message });
        }
    }

    async open(file) {
        const epoch = ++this.epoch;
        this.stopDecoder();
        this.keepBeforeTarget(null);
        const Demux = this.demux.constructor;
        this.demux.free();
        this.demux = new Demux();
        this.file = file;
        this.events = [];
        this.openedFile = true;
        this.ready = false;
        this.announced = false;
        try {
            if (typeof VideoDecoder !== "function") {
                throw new Error("H.264 WebCodecs is unavailable. Use a recent Chrome or Edge build over HTTPS or localhost.");
            }
            let last = -Infinity;
            const recent = [];
            const keys = [];
            const takeIndex = () => {
                let unit;
                while ((unit = this.demux.next())) {
                    const { pts, key, offset } = unit;
                    if (pts > last && Number.isFinite(last)) {
                        recent.push(pts - last);
                        if (recent.length > 15) recent.shift();
                    }
                    last = Math.max(last, pts);
                    if (key) keys.push({ pts, offset });
                    unit.free();
                }
            };
            for (let offset = 0; offset < file.size; offset += BLOCK) {
                const bytes = new Uint8Array(await file.slice(offset, offset + BLOCK).arrayBuffer());
                if (epoch !== this.epoch) return;
                this.demux.push(bytes, offset);
                takeIndex();
            }
            this.demux.flush();
            takeIndex();
            const codec = this.demux.codec();
            const first = this.demux.start();
            if (keys.length === 0 || !codec || !Number.isFinite(first)) {
                throw new Error("No playable H.264 MPEG-TS video was found.");
            }
            recent.sort((a, b) => a - b);
            this.start = first;
            this.duration = last - first + (recent[Math.floor(recent.length / 2)] || 1 / 30);
            this.keys = keys.sort((a, b) => a.pts - b.pts);
            this.codec = codec;
            this.software = false;
            this.ready = true;
            if (this.demux.problems()) this.report("warning", "The recording contains damaged transport packets.");
            await this.restart(0);
        } catch (error) {
            if (epoch === this.epoch) this.report("error", String(error));
        }
    }

    seek(position) {
        if (!this.ready || !Number.isFinite(position) || position < 0 || position >= this.duration) {
            this.report("warning", "Requested time is outside the video.");
            return;
        }
        this.events = [];
        void this.restart(position);
    }

    async restart(position) {
        const epoch = ++this.epoch;
        this.stopDecoder();
        this.keepBeforeTarget(null);
        this.position = position;
        this.copies = 0;
        this.pumping = false;
        this.eof = false;
        this.flushed = false;
        this.firstKey = true;
        this.delivered = 0;
        this.pending = Promise.resolve();
        let lo = 0;
        let hi = this.keys.length;
        while (lo < hi) {
            const mid = (lo + hi) >>> 1;
            if (this.keys[mid].pts <= position + this.start) lo = mid + 1;
            else hi = mid;
        }
        const key = this.keys[Math.max(lo - 1, 0)];
        this.offset = key.offset;
        this.demux.seek(key.pts);
        try {
            await this.configure(epoch);
            if (epoch === this.epoch) this.kick();
        } catch (error) {
            if (epoch === this.epoch) this.report("error", String(error));
        }
    }

    async configure(epoch) {
        const config = {
            codec: this.codec,
            optimizeForLatency: true,
            hardwareAcceleration: this.software ? "prefer-software" : "prefer-hardware",
        };
        let result = await VideoDecoder.isConfigSupported(config);
        if (!result.supported && !this.software) {
            config.hardwareAcceleration = "no-preference";
            result = await VideoDecoder.isConfigSupported(config);
        }
        if (epoch !== this.epoch) return;
        if (!result.supported) throw new Error("This browser cannot decode the source H.264 profile.");
        this.decoder = new VideoDecoder({
            output: frame => this.copyFrame(epoch, frame),
            error: error => this.decoderError(epoch, error),
        });
        this.decoder.configure(config);
        this.decoder.addEventListener("dequeue", () => this.kick());
    }

    decoderError(epoch, error) {
        if (epoch !== this.epoch) return;
        if (this.delivered === 0 && !this.software) {
            this.software = true;
            this.report("warning", "Retrying with software H.264 decoding.");
            void this.restart(this.position);
            return;
        }
        this.report("warning", `Damaged or unsupported H.264 frame skipped: ${error.message}`);
        if (this.delivered === 0) {
            this.report("error", "WebCodecs could not decode the recording.");
            this.eof = true;
            return;
        }
        this.firstKey = true;
        this.stopDecoder();
        void this.configure(epoch).then(() => this.kick()).catch(fault => {
            if (epoch === this.epoch) this.report("error", String(fault));
        });
    }

    keepBeforeTarget(frame) {
        this.beforeTarget?.close();
        this.beforeTarget = frame;
    }

    copyFrame(epoch, frame, pts = frame.timestamp / 1e6) {
        if (epoch !== this.epoch) {
            frame.close();
            return;
        }
        if (pts + 0.001 < this.position) {
            this.keepBeforeTarget(frame);
            return;
        }
        this.keepBeforeTarget(null);
        this.copies++;
        this.pending = this.pending.then(async () => {
            try {
                if (epoch !== this.epoch) return;
                const width = frame.visibleRect.width;
                const height = frame.visibleRect.height;
                if (width <= 2 || height <= 2 || width > 4096 || height > 4096 ||
                    width % 2 || height % 2 || width * 1080 !== height * 1920) {
                    throw new Error(`Unsupported camera mosaic: ${width} × ${height}`);
                }
                const data = new Uint8Array(width * height * 4);
                await frame.copyTo(data, { format: "RGBA", colorSpace: "srgb", layout: [{ offset: 0, stride: width * 4 }] });
                if (epoch !== this.epoch) return;
                if (!this.announced) {
                    this.announced = true;
                    this.events.push({ kind: "opened", width, height, name: this.file.name, duration: this.duration });
                }
                this.delivered++;
                this.events.push({ kind: "frame", pts, width, height, data });
            } catch (error) {
                if (epoch === this.epoch) this.report("error", String(error));
            } finally {
                frame.close();
                if (epoch === this.epoch) {
                    this.copies--;
                    this.kick();
                }
            }
        });
    }

    poll() {
        const event = this.events.shift() ?? null;
        this.kick();
        return event;
    }

    kick() {
        if (this.pumping || this.eof || this.decoder?.state !== "configured") return;
        const epoch = this.epoch;
        this.pumping = true;
        void this.pump(epoch).catch(error => {
            if (epoch === this.epoch && !this.eof && this.decoder?.state === "configured") {
                this.report("error", String(error));
            }
        }).finally(() => {
            if (epoch === this.epoch) this.pumping = false;
        });
    }

    async pump(epoch) {
        while (epoch === this.epoch && this.decoder?.state === "configured" &&
               this.copies + this.decoder.decodeQueueSize + this.events.filter(event => event.kind === "frame").length < IN_FLIGHT) {
            const unit = this.demux.next();
            if (unit) {
                try {
                    if (this.firstKey && !unit.key) continue;
                    this.firstKey = false;
                    this.decoder.decode(new EncodedVideoChunk({
                        type: unit.key ? "key" : "delta",
                        timestamp: Math.round((unit.pts - this.start) * 1e6),
                        data: unit.take(),
                    }));
                } finally {
                    unit.free();
                }
                continue;
            }
            if (this.demux.problems()) this.report("warning", "The recording contains damaged transport packets.");
            if (this.offset < this.file.size) {
                const offset = this.offset;
                const bytes = new Uint8Array(await this.file.slice(offset, offset + BLOCK).arrayBuffer());
                if (epoch !== this.epoch) return;
                this.demux.push(bytes, offset);
                this.offset += bytes.length;
            } else if (!this.flushed) {
                this.flushed = true;
                this.demux.flush();
            } else {
                this.eof = true;
                try {
                    await this.decoder.flush();
                    await this.pending;
                    if (epoch === this.epoch && this.delivered === 0 && this.beforeTarget) {
                        const frame = this.beforeTarget;
                        this.beforeTarget = null;
                        this.copyFrame(epoch, frame, this.position);
                        await this.pending;
                    }
                } catch (error) {
                    if (epoch === this.epoch) this.report("warning", `Video ended at a damaged frame: ${error.message}`);
                }
                if (epoch === this.epoch) this.events.push({ kind: "end" });
                break;
            }
        }
    }
}
