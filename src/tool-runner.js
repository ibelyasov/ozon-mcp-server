// One tool at a time: a description retry must not close another tool's browser.
export function createToolRunner({ timeoutMs = 55000, maxText = 60000 } = {}) {
  let tail = Promise.resolve();
  return function run(label, operation, { signal: parentSignal } = {}) {
    const controller = new AbortController();
    const abort = () => controller.abort(parentSignal.reason || new Error('Request cancelled'));
    if (parentSignal?.aborted) abort();
    else parentSignal?.addEventListener('abort', abort, { once: true });
    const timer = setTimeout(() => controller.abort(new Error(`${label} timed out after ${timeoutMs}ms`)), timeoutMs);
    const signal = controller.signal;
    let rejectAbort;
    const aborted = new Promise((_, reject) => { rejectAbort = () => reject(signal.reason); });
    if (signal.aborted) rejectAbort();
    else signal.addEventListener('abort', rejectAbort, { once: true });
    const work = tail.then(() => {
      signal.throwIfAborted();
      return operation(signal);
    });
    // Keep ownership until actual work settles, even if the caller timed out.
    tail = work.catch(() => {});
    return Promise.race([work, aborted]).then(result => {
      const text = JSON.stringify(result, null, 2);
      if (typeof text !== 'string') throw new Error('Tool returned no result');
      // Never cut JSON in the middle: return a valid error instead of corrupt data.
      if (text.length > maxText) throw new Error('RESULT_TOO_LARGE: response exceeds size limit');
      return { content: [{ type: 'text', text }] };
    }).catch(error => ({
      content: [{ type: 'text', text: `Error: ${String(error?.message || 'Request failed').replace(/[\u0000-\u001f\u007f]/g, ' ')}`.slice(0, Math.min(maxText, 1000)) }], isError: true,
    })).finally(() => {
      clearTimeout(timer);
      signal.removeEventListener('abort', rejectAbort);
      parentSignal?.removeEventListener('abort', abort);
    });
  };
}
