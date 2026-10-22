//! Go's `sort.SliceStable`, step for step.
//!
//! Several of the pool's orderings are "a before b" rules that are not a strict
//! weak ordering in every case (an identity representative is chosen by
//! cooldown, then quota class, then priority, with ties broken differently by
//! different fields). Go's stable sort never fails on such a rule and produces
//! a definite order; Rust's `sort_by` may panic when the comparison is not a
//! total order and, where it does not, may order the elements differently.
//! Ported orderings therefore sort with Go's own algorithm (sort/zsortfunc.go
//! stable_func: insertion sort over blocks of 20, then SymMerge), so they put
//! accounts in the same order Go did, ties and inconsistent rules included.

pub fn slice_stable<T>(data: &mut [T], mut less: impl FnMut(&T, &T) -> bool) {
    let n = data.len();
    let mut less_at = |data: &[T], i: usize, j: usize| less(&data[i], &data[j]);
    let mut block = 20;
    let (mut a, mut b) = (0, block);
    while b <= n {
        insertion_sort(data, a, b, &mut less_at);
        a = b;
        b += block;
    }
    insertion_sort(data, a, n, &mut less_at);
    while block < n {
        a = 0;
        b = 2 * block;
        while b <= n {
            sym_merge(data, a, a + block, b, &mut less_at);
            a = b;
            b += 2 * block;
        }
        let m = a + block;
        if m < n {
            sym_merge(data, a, m, n, &mut less_at);
        }
        block *= 2;
    }
}

type Less<'a, T> = dyn FnMut(&[T], usize, usize) -> bool + 'a;

fn insertion_sort<T>(data: &mut [T], a: usize, b: usize, less: &mut Less<'_, T>) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && less(data, j, j - 1) {
            data.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sym_merge<T>(data: &mut [T], a: usize, m: usize, b: usize, less: &mut Less<'_, T>) {
    if m - a == 1 {
        let (mut i, mut j) = (m, b);
        while i < j {
            let h = (i + j) >> 1;
            if less(data, h, a) {
                i = h + 1;
            } else {
                j = h;
            }
        }
        let mut k = a;
        while k + 1 < i {
            data.swap(k, k + 1);
            k += 1;
        }
        return;
    }
    if b - m == 1 {
        let (mut i, mut j) = (a, m);
        while i < j {
            let h = (i + j) >> 1;
            if !less(data, m, h) {
                i = h + 1;
            } else {
                j = h;
            }
        }
        let mut k = m;
        while k > i {
            data.swap(k, k - 1);
            k -= 1;
        }
        return;
    }
    let mid = (a + b) >> 1;
    let n = mid + m;
    let (mut start, mut r) = if m > mid { (n - b, mid) } else { (a, m) };
    let p = n - 1;
    while start < r {
        let c = (start + r) >> 1;
        if !less(data, p - c, c) {
            start = c + 1;
        } else {
            r = c;
        }
    }
    let end = n - start;
    if start < m && m < end {
        rotate(data, start, m, end);
    }
    if a < start && start < mid {
        sym_merge(data, a, start, mid, less);
    }
    if mid < end && end < b {
        sym_merge(data, mid, end, b, less);
    }
}

fn rotate<T>(data: &mut [T], a: usize, m: usize, b: usize) {
    let mut i = m - a;
    let mut j = b - m;
    while i != j {
        if i > j {
            swap_range(data, m - i, m, j);
            i -= j;
        } else {
            swap_range(data, m - i, m + j - i, i);
            j -= i;
        }
    }
    swap_range(data, m - i, m, i);
}

fn swap_range<T>(data: &mut [T], a: usize, b: usize, n: usize) {
    for i in 0..n {
        data.swap(a + i, b + i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stable: equal keys keep their order, across the block merges too.
    #[test]
    fn sorts_stably_at_every_size() {
        for n in [0usize, 1, 2, 19, 20, 21, 45, 100, 257] {
            let mut items: Vec<(u64, usize)> = (0..n)
                .map(|i| ((i as u64).wrapping_mul(2_654_435_761) % 7, i))
                .collect();
            slice_stable(&mut items, |a, b| a.0 < b.0);
            for pair in items.windows(2) {
                assert!(pair[0].0 < pair[1].0 || (pair[0].0 == pair[1].0 && pair[0].1 < pair[1].1));
            }
        }
    }

    // A rule that is not a total order yields the order Go's SliceStable
    // gives it, without panicking. Expected output from Go 1.24.
    #[test]
    fn matches_go_on_an_inconsistent_rule() {
        let mut items: Vec<u32> = (0..60).rev().collect();
        slice_stable(&mut items, |a, b| (a % 3 == 0) != (b % 5 == 0));
        assert_eq!(
            items,
            [
                15, 12, 57, 59, 58, 56, 55, 18, 36, 39, 51, 54, 53, 52, 49, 19, 17, 16, 14, 13, 11,
                10, 6, 9, 8, 7, 4, 5, 0, 3, 38, 37, 34, 35, 30, 27, 33, 32, 50, 45, 42, 48, 47, 46,
                44, 43, 41, 31, 29, 28, 26, 2, 25, 40, 21, 24, 23, 22, 1, 20
            ]
        );
        let mut items: Vec<u32> = (0..45).map(|i| (i * 37) % 11).collect();
        slice_stable(&mut items, |a, b| a % 4 < b % 4);
        assert_eq!(
            items,
            [
                0, 4, 8, 0, 4, 8, 0, 4, 8, 0, 4, 8, 0, 1, 5, 9, 1, 5, 9, 1, 5, 9, 1, 5, 9, 2, 6,
                10, 2, 6, 10, 2, 6, 10, 2, 6, 10, 3, 7, 3, 7, 3, 7, 3, 7
            ]
        );
    }
}
