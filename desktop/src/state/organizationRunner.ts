import type { Job } from '../organization';
/** One durable item per turn. Stopping never pretends an in-flight commit was undone. */
export class OrganizationRunner {
  private stopped = false;
  private active = false;
  stop() { this.stopped = true; }
  async run(initial: Job, step: (job: string) => Promise<Job>, progress: (job: Job) => void): Promise<void> {
    if (this.active) throw new Error('A batch is already running.');
    this.active = true; this.stopped = false;
    try {
      let job = initial;
      while (!this.stopped && ['ready', 'running'].includes(job.state)) {
        job = await step(job.id); progress(job);
        await new Promise<void>(resolve => setTimeout(resolve, 0));
      }
    } finally { this.active = false; }
  }
}
