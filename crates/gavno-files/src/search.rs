use crate::{model, ops};
use gio::prelude::*;
use std::collections::VecDeque;
pub const LIMIT: usize = 10_000;

pub async fn recursive(
    root: gio::File,
    query: String,
    hidden: bool,
    cancel: gio::Cancellable,
    add: impl Fn(Vec<gio::FileInfo>),
    error: impl Fn(String),
) -> Result<bool, glib::Error> {
    let query = query.to_lowercase();
    let mut queue = VecDeque::from([root]);
    let mut count = 0;
    while let Some(dir) = queue.pop_front() {
        cancel.set_error_if_cancelled()?;
        let enumerator = match ops::enumerate(&dir, model::ATTRIBUTES, &cancel).await {
            Ok(e) => e,
            Err(e) if e.matches(gio::IOErrorEnum::Cancelled) => return Err(e),
            Err(e) => {
                error(e.to_string());
                continue;
            }
        };
        loop {
            let batch = ops::next(&enumerator, &cancel).await?;
            if batch.is_empty() {
                break;
            }
            let mut matches = Vec::new();
            for info in batch {
                if !hidden && info.is_hidden() {
                    continue;
                }
                let file = enumerator.child(&info);
                model::attach(&info, &file);
                // NOFOLLOW_SYMLINKS prevents cycles and crossing symlink trees.
                if model::is_dir(&info) && !info.is_symlink() {
                    queue.push_back(file);
                }
                if info.display_name().to_lowercase().contains(&query) {
                    matches.push(info);
                    count += 1;
                    if count == LIMIT {
                        add(matches);
                        enumerator.close_future(glib::Priority::DEFAULT).await?;
                        return Ok(true);
                    }
                }
            }
            add(matches);
            glib::timeout_future(std::time::Duration::from_millis(1)).await;
        }
        enumerator.close_future(glib::Priority::DEFAULT).await?;
    }
    Ok(false)
}
