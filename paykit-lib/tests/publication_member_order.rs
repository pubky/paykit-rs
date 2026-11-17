use paykit_lib::publications::PrivateListPublication;
use paykit_lib::records::UnknownMembers;
use paykit_lib::ListVersion;

#[test]
fn test_publication_unknown_members_keep_known_field_order() {
    let value = PrivateListPublication {
        publisher: [1; 16],
        recipient: [2; 32],
        version: ListVersion::new(7, 12).unwrap(),
        grant_id: None,
        message_id: "d0efffb7-191b-4709-b6c7-e343af5500b4".into(),
        message_hash: [4; 32],
        unknown: UnknownMembers::default(),
    };
    let mut object: serde_json::Value = serde_json::from_slice(&value.to_json().unwrap()).unwrap();
    object["future"] = true.into();
    let parsed = PrivateListPublication::read(&serde_json::to_vec(&object).unwrap()).unwrap();
    let written = String::from_utf8(parsed.to_json().unwrap()).unwrap();
    let positions: Vec<usize> = [
        "v",
        "publisher",
        "recipient",
        "generation",
        "counter",
        "grant_id",
        "message_id",
        "message_hash",
    ]
    .iter()
    .map(|field| written.find(&format!("\"{field}\":")).unwrap())
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "required member order changed: {written}"
    );
}
