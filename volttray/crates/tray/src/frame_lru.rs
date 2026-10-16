//! 小容量帧 LRU（v0.3.15，P1-2）。
//!
//! 图标帧从「启动全量预渲染常驻」（4 色档 × 103 文本 × 2 提亮 = 824 帧
//! RGBA，16px≈0.8MB / 32px≈3.3MB / 48px≈7.4MB 堆）改为懒渲染后，用
//! 「逻辑时钟 + 容量上限」约束常驻内存：只有实际出现过的帧才占内存，
//! 超出容量淘汰最久未命中的帧。纯逻辑、无 Win32 依赖，跨平台可测。
//!
//! 淘汰扫描是 O(n)：容量仅 32（见 icon.rs `FRAME_CACHE_CAP` 论证），
//! 每 500ms 至多一次，代价可忽略。

use std::collections::HashMap;
use std::hash::Hash;

pub struct FrameLru<K> {
    cap: usize,
    tick: u64,
    misses: u64,
    map: HashMap<K, (u64, Vec<u8>)>,
}

impl<K: Eq + Hash + Clone> FrameLru<K> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            tick: 0,
            misses: 0,
            map: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 渲染 miss 累计（测试与诊断用：命中不计）。
    pub fn miss_count(&self) -> u64 {
        self.misses
    }

    /// 只查不碰（不推进命中时钟——调用方据此决定要不要先渲染）。
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// 命中：刷新逻辑时钟并返回帧；未命中：调用 `render` 渲染、按容量
    /// 淘汰最旧帧后插入。返回的引用有效期与 self 借用一致。
    pub fn get_or_insert_with(&mut self, key: K, render: impl FnOnce() -> Vec<u8>) -> &[u8] {
        self.tick += 1;
        let now = self.tick;
        if let Some((t, _)) = self.map.get_mut(&key) {
            *t = now;
        } else {
            self.misses += 1;
            let frame = render();
            if self.map.len() >= self.cap {
                // 淘汰逻辑时钟最旧的帧。
                if let Some(oldest) = self
                    .map
                    .iter()
                    .min_by_key(|(_, (t, _))| *t)
                    .map(|(k, _)| k.clone())
                {
                    self.map.remove(&oldest);
                }
            }
            self.map.insert(key.clone(), (now, frame));
        }
        &self.map.get(&key).expect("just inserted or hit").1
    }

    /// DPI / 渲染输入变更时整体失效（尺寸、配色都变了，旧帧无一可复用）。
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(tag: u8) -> Vec<u8> {
        vec![tag; 4]
    }

    /// 懒渲染语义：未命中才调 render；命中不重复渲染且刷新淘汰时钟。
    #[test]
    fn hit_does_not_rerender() {
        let mut lru: FrameLru<&str> = FrameLru::new(4);
        let mut renders = 0u32;
        let first = lru
            .get_or_insert_with("a", || {
                renders += 1;
                frame(1)
            })
            .to_vec();
        let second = lru
            .get_or_insert_with("a", || {
                renders += 1;
                frame(2)
            })
            .to_vec();
        assert_eq!(first, frame(1));
        assert_eq!(second, frame(1), "命中必须返回首次渲染的帧");
        assert_eq!(renders, 1, "命中不得重复渲染");
        assert_eq!(lru.miss_count(), 1);
    }

    /// 超容量淘汰最久未命中的帧；刚被命中的帧不被淘汰。
    #[test]
    fn evicts_least_recently_used() {
        let mut lru: FrameLru<&str> = FrameLru::new(2);
        let _ = lru.get_or_insert_with("a", || frame(1));
        let _ = lru.get_or_insert_with("b", || frame(2));
        // 命中 a：b 变为最旧。
        let _ = lru.get_or_insert_with("a", || frame(9));
        // 插入 c：淘汰 b。
        let _ = lru.get_or_insert_with("c", || frame(3));
        assert!(lru.contains(&"a"));
        assert!(!lru.contains(&"b"), "最久未命中的 b 应被淘汰");
        assert!(lru.contains(&"c"));
        assert_eq!(lru.len(), 2);
    }

    /// 闪烁两相位场景还原：双键交替命中 100 次无淘汰、无重复渲染。
    #[test]
    fn alternating_blink_phases_stay_cached() {
        let mut lru: FrameLru<bool> = FrameLru::new(4);
        let mut renders = 0u32;
        for _ in 0..100 {
            for bright in [false, true] {
                let _ = lru.get_or_insert_with(bright, || {
                    renders += 1;
                    frame(bright as u8)
                });
            }
        }
        assert_eq!(renders, 2, "两相位各渲染一次后全程命中");
        assert_eq!(lru.len(), 2);
    }

    /// clear 后全部 miss；容量下限钳制为 1。
    #[test]
    fn clear_invalidates_and_cap_floors_at_one() {
        let mut lru: FrameLru<&str> = FrameLru::new(0);
        let _ = lru.get_or_insert_with("x", || frame(1));
        let _ = lru.get_or_insert_with("y", || frame(2));
        assert_eq!(lru.len(), 1, "cap 钳制为 1：插入 y 淘汰 x");
        assert!(!lru.contains(&"x"));
        lru.clear();
        assert_eq!(lru.len(), 0);
        let misses_before = lru.miss_count();
        let _ = lru.get_or_insert_with("y", || frame(3));
        assert_eq!(lru.miss_count(), misses_before + 1, "clear 后重新渲染");
    }
}
