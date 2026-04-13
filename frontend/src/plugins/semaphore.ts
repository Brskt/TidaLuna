/**
 * Minimal counting Semaphore - drop-in for @inrixia/helpers Semaphore.
 * Used by LunaPlugin to prevent concurrent loads.
 */
export class Semaphore {
    private permits: number;
    private queue: (() => void)[] = [];

    constructor(permits: number) {
        this.permits = permits;
    }

    async acquire(): Promise<void> {
        if (this.permits > 0) {
            this.permits--;
            return;
        }
        return new Promise<void>((resolve) => {
            this.queue.push(resolve);
        });
    }

    release(): void {
        const next = this.queue.shift();
        if (next) {
            next();
        } else {
            this.permits++;
        }
    }
}
