use std::collections::VecDeque;
use std::sync::{mpsc, Mutex};

pub(crate) fn job_count(requested: usize, auto_cap: usize) -> usize {
    if requested > 0 {
        return requested.max(1);
    }

    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, auto_cap.max(1))
}

pub(crate) fn map_ordered<T, R, F>(items: Vec<T>, jobs: usize, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    let len = items.len();
    if len == 0 {
        return Vec::new();
    }

    let jobs = jobs.max(1).min(len);
    if jobs == 1 {
        return items.into_iter().map(f).collect();
    }

    let queue: Mutex<VecDeque<(usize, T)>> = Mutex::new(items.into_iter().enumerate().collect());
    let (tx, rx) = mpsc::channel();

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let tx = tx.clone();
            let queue = &queue;
            let f = &f;

            scope.spawn(move || loop {
                let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                let Some((index, item)) = next else {
                    break;
                };
                if tx.send((index, f(item))).is_err() {
                    break;
                }
            });
        }
        drop(tx);

        let mut results = Vec::with_capacity(len);
        results.resize_with(len, || None);
        for (index, result) in rx {
            results[index] = Some(result);
        }

        results
            .into_iter()
            .map(|result| result.expect("parallel worker did not return a result"))
            .collect()
    })
}
