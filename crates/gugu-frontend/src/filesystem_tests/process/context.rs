use super::*;

pub(super) fn windows_command_line_round_trips_empty_quotes_slashes_and_surrogates() {
    let (hir, bodies) = super::super::harness::runtime_bodies_for(TargetName::X86_64Windows);
    let mut h = Harness::from_bodies(hir, bodies);
    let mut m = vec![0; 131072];
    let command = r#""C:\app name\probe.exe" "" "a b" "x\"y" "tail\\" "汉字🙂" "$();""#;
    let bytes: Vec<_> = command
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    m[1024..1024 + bytes.len()].copy_from_slice(&bytes);
    let expected = [
        r"C:\app name\probe.exe",
        "",
        "a b",
        "x\"y",
        "tail\\",
        "汉字🙂",
        "$();",
    ];
    let measured = h.invoke(CONTEXT, "parse_command_line", &[1024, 0, 0], &mut m);
    let result = h.invoke(CONTEXT, "parse_command_line", &[1024, 512, 2048], &mut m);
    assert_eq!(result, measured);
    assert_eq!(result[0], u64::try_from(expected.len()).unwrap());
    for (index, text) in expected.iter().enumerate() {
        let at = usize::try_from(word(&m, 512 + u64::try_from(index).unwrap() * 8)).unwrap();
        let encoded: Vec<_> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(&m[at..at + encoded.len()], encoded);
    }
    for (units, valid) in [
        (&[0xd83d, 0xde42][..], 1),
        (&[0xd83d][..], 0),
        (&[0xdc00][..], 0),
        (&[0xd800, 65][..], 0),
    ] {
        for (index, unit) in units.iter().enumerate() {
            m[1024 + index * 2..1026 + index * 2]
                .copy_from_slice(&u16::try_from(*unit).unwrap().to_le_bytes());
        }
        assert_eq!(
            h.invoke(
                CONTEXT,
                "valid_native",
                &[1024, u64::try_from(units.len() * 2).unwrap()],
                &mut m
            ),
            [valid]
        );
    }
}

pub(super) fn environment_names_reject_invalid_bytes_before_any_native_call(output: &Protocol) {
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    for bytes in [&b""[..], b"A=B", b"A\0B", b"A", b"a", &[255]] {
        m[1024..1024 + bytes.len()].copy_from_slice(bytes);
        store(&mut m, 512, &[1024, u64::try_from(bytes.len()).unwrap()]);
        let result = h.invoke(
            "std/runtime/environment.gg",
            "validate_name",
            &[512],
            &mut m,
        );
        assert_eq!(
            result[0],
            u64::from(bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'='))
        );
    }
    assert!(h.calls.is_empty());
    assert!(h.allocations.is_empty());
}
