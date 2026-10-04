/// Reports an operation finished exactly once: when the work completes, or
/// (as failed) when its future is dropped first. The compute engine drops a
/// computation nobody needs any more; without this its progress entry would
/// stay queued, and on screen, for the rest of the command.
pub(crate) struct FinishOnDrop<F: FnOnce(bool)>(Option<F>);

impl<F: FnOnce(bool)> FinishOnDrop<F> {
    pub(crate) fn new(on_finished: Option<F>) -> Self {
        Self(on_finished)
    }

    pub(crate) fn finish(&mut self, failed: bool) {
        if let Some(on_finished) = self.0.take() {
            on_finished(failed);
        }
    }
}

impl<F: FnOnce(bool)> Drop for FinishOnDrop<F> {
    fn drop(&mut self) {
        self.finish(true);
    }
}
