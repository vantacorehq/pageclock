use pageclock::{Error, Pool};

fn load_id(id: u64) -> impl FnOnce(&mut [u8]) -> Result<(), ()> {
    move |dst: &mut [u8]| {
        dst[..8].copy_from_slice(&id.to_le_bytes());
        Ok(())
    }
}

fn ok_flush(_: u64, _: &[u8]) -> Result<(), ()> {
    Ok(())
}

#[test]
fn hit_does_not_reload() {
    let mut pool: Pool<32, 2> = Pool::new();
    let mut loads = 0u32;
    let frame = pool
        .pin(
            4,
            |dst| {
                loads += 1;
                dst[..8].copy_from_slice(&4u64.to_le_bytes());
                Ok(())
            },
            ok_flush,
        )
        .unwrap();
    pool.unpin(frame).unwrap();
    let again = pool.pin(4, load_id(4), ok_flush).unwrap();
    assert_eq!(frame, again);
    assert_eq!(loads, 1);
    assert_eq!(pool.stats().hits, 1);
    assert_eq!(pool.stats().misses, 1);
    assert_eq!(pool.pins(frame).unwrap(), 1);
}

#[test]
fn double_pin_and_extra_unpin() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    assert_eq!(pool.pin(1, load_id(1), ok_flush).unwrap(), frame);
    assert_eq!(pool.pins(frame).unwrap(), 2);
    pool.unpin(frame).unwrap();
    pool.unpin(frame).unwrap();
    assert_eq!(pool.pins(frame).unwrap(), 0);
    assert_eq!(pool.unpin(frame), Err(Error::NotPinned));
}

#[test]
fn pinned_page_is_not_evicted() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    let mut loaded = false;
    let err = pool.pin(
        2,
        |_| {
            loaded = true;
            Ok(())
        },
        ok_flush,
    );
    assert_eq!(err, Err(Error::Full));
    assert!(!loaded);
    assert_eq!(pool.page_id(frame).unwrap(), 1);
    assert_eq!(pool.resident_count(), 1);
}

#[test]
fn second_chance_then_evict_the_cold_page() {
    let mut pool: Pool<32, 2> = Pool::new();
    let a = pool.pin(1, load_id(1), ok_flush).unwrap();
    pool.unpin(a).unwrap();
    let b = pool.pin(2, load_id(2), ok_flush).unwrap();
    pool.unpin(b).unwrap();

    let c = pool.pin(3, load_id(3), ok_flush).unwrap();
    assert_eq!(pool.page_id(c).unwrap(), 3);
    assert_eq!(pool.stats().evictions, 1);
    assert!(pool.bytes(0).unwrap()[..8] == 3u64.to_le_bytes() || pool.bytes(1).unwrap()[..8] == 3u64.to_le_bytes());
    let survivor = if pool.page_id(0).ok() == Some(2) { 0 } else { 1 };
    assert_eq!(pool.page_id(survivor).unwrap(), 2);
    assert_eq!(&pool.bytes(survivor).unwrap()[..8], &2u64.to_le_bytes());
}

#[test]
fn dirty_eviction_flushes_exact_bytes() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    pool.bytes_mut(frame).unwrap()[..4].copy_from_slice(&[9, 9, 9, 9]);
    pool.unpin(frame).unwrap();

    let mut flushed = None;
    let frame = pool
        .pin(
            2,
            load_id(2),
            |id, bytes| {
                flushed = Some((id, bytes[..4].to_vec()));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(flushed, Some((1, vec![9, 9, 9, 9])));
    assert_eq!(pool.stats().flushes, 1);
    assert_eq!(pool.page_id(frame).unwrap(), 2);
    assert!(!pool.is_dirty(frame).unwrap());
}

#[test]
fn failed_flush_keeps_the_dirty_page() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    pool.mark_dirty(frame).unwrap();
    pool.unpin(frame).unwrap();

    let err = pool.pin(2, load_id(2), |_id, _bytes| Err(()));
    assert_eq!(err, Err(Error::Flush));
    assert_eq!(pool.page_id(frame).unwrap(), 1);
    assert!(pool.is_dirty(frame).unwrap());
    assert_eq!(pool.stats().evictions, 0);
    assert_eq!(pool.stats().misses, 1);

    let again = pool.pin(1, load_id(99), ok_flush).unwrap();
    assert_eq!(again, frame);
    assert_eq!(pool.stats().hits, 1);
}

#[test]
fn failed_load_does_not_drop_resident_page() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    pool.unpin(frame).unwrap();

    let err = pool.pin(2, |_| Err(()), ok_flush);
    assert_eq!(err, Err(Error::Load));
    assert_eq!(pool.page_id(frame).unwrap(), 1);
    assert_eq!(&pool.bytes(frame).unwrap()[..8], &1u64.to_le_bytes());
    assert_eq!(pool.stats().evictions, 0);
    assert_eq!(pool.resident_count(), 1);
}

#[test]
fn clean_eviction_does_not_flush() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    pool.unpin(frame).unwrap();
    let mut flushed = false;
    pool.pin(2, load_id(2), |_id, _bytes| {
        flushed = true;
        Ok(())
    })
    .unwrap();
    assert!(!flushed);
    assert_eq!(pool.stats().flushes, 0);
    assert_eq!(pool.stats().evictions, 1);
}

#[test]
fn seal_and_verify_detect_a_flipped_bit() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    pool.seal(frame).unwrap();
    pool.verify(frame).unwrap();
    pool.bytes_mut(frame).unwrap()[0] ^= 0x01;
    assert_eq!(pool.verify(frame), Err(Error::Checksum));
}

#[test]
fn churn_keeps_resident_images_intact() {
    let mut pool: Pool<64, 4> = Pool::new();
    let mut live: [Option<u64>; 4] = [None; 4];
    for step in 0..200u64 {
        let id = step % 7;
        let frame = pool.pin(id, load_id(id), ok_flush).unwrap();
        assert_eq!(&pool.bytes(frame).unwrap()[..8], &id.to_le_bytes());
        if let Some(previous) = live[frame] {
            if previous != id {
                let _ = pool.page_id(frame);
            }
        }
        live[frame] = Some(id);
        if step % 2 == 0 {
            pool.unpin(frame).unwrap();
            live[frame] = Some(id);
        } else if pool.pins(frame).unwrap() > 1 {
            pool.unpin(frame).unwrap();
        }
        if step % 5 == 0 {
            for index in 0..4 {
                if pool.pins(index).unwrap_or(0) > 0 {
                    pool.unpin(index).unwrap();
                }
            }
        }
    }
    for index in 0..4 {
        if let Ok(id) = pool.page_id(index) {
            assert_eq!(&pool.bytes(index).unwrap()[..8], &id.to_le_bytes());
        }
    }
}

#[test]
fn bytes_mut_requires_a_pin() {
    let mut pool: Pool<32, 1> = Pool::new();
    let frame = pool.pin(1, load_id(1), ok_flush).unwrap();
    pool.unpin(frame).unwrap();
    assert_eq!(pool.bytes_mut(frame).err(), Some(Error::NotPinned));
    assert!(pool.bytes(frame).is_ok());
}
