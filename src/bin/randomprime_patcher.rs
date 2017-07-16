extern crate memmap;
#[macro_use]
extern crate clap;
extern crate rand;
extern crate randomprime;

use clap::{Arg, App};
// XXX This is an undocumented enum
use clap::Format;
use rand::{XorShiftRng, SeedableRng, Rng, Rand};

pub use randomprime::*;

use asset_ids;
use reader_writer::{FourCC, Reader, Writable};
use reader_writer::generic_array::GenericArray;
use reader_writer::typenum::U3;
use reader_writer::num::{BigUint, Integer, ToPrimitive};

use std::io;
use std::panic;
use std::iter;
use std::ascii::AsciiExt;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::ops::RangeFrom;

const METROID_PAK_NAMES: [&'static str; 5] = [
    "Metroid2.pak",
    "Metroid3.pak",
    "Metroid4.pak",
    "metroid5.pak",
    "Metroid6.pak",
];

const ARTIFACT_OF_TRUTH_REQ_LAYER: u32 = 24;

fn write_gc_disc(gc_disc: &mut structs::GcDisc, path: &str, mut pn: ProgressNotifier)
    -> Result<(), String>
{
    let mut out_iso = OpenOptions::new()
        .write(true)
        .create(true)
        .open(path)
        .map_err(|e| format!("Failed to open output file: {}", e))?;
    out_iso.set_len(structs::GC_DISC_LENGTH as u64)
        .map_err(|e| format!("Failed to open output file: {}", e))?;

    gc_disc.write(&mut out_iso, &mut pn)
        .map_err(|e| format!("Error writing output file: {}", e))?;

    pn.notify_flushing_to_disk();
    Ok(())
}

// When changing a pickup, we need to give the room a copy of the resources/
// assests used by the pickup. Create a cache of all the resources needed by
// any pickup.
fn collect_pickup_resources<'a>(gc_disc: &structs::GcDisc<'a>)
    -> HashMap<(u32, FourCC), structs::Resource<'a>>
{
    let mut looking_for: HashSet<_> = pickup_meta::pickup_meta_table().iter()
        .flat_map(|meta| meta.deps.iter().map(|key| *key))
        .collect();

    let mut found = HashMap::with_capacity(looking_for.len());

    let extra_assets = pickup_meta::extra_assets();
    for res in extra_assets {
        looking_for.remove(&(res.file_id, res.fourcc()));
        assert!(found.insert((res.file_id, res.fourcc()), res.clone()).is_none());
    }

    for pak_name in METROID_PAK_NAMES.iter() {
        let file_entry = find_file(gc_disc, pak_name);
        let pak = match *file_entry.file().unwrap() {
            structs::FstEntryFile::Pak(ref pak) => Cow::Borrowed(pak),
            structs::FstEntryFile::Unknown(ref reader) => Cow::Owned(reader.clone().read(())),
            _ => panic!(),
        };


        for res in pak.resources.iter() {
            let key = (res.file_id, res.fourcc());
            if looking_for.remove(&key) {
                assert!(found.insert(key, res.into_owned()).is_none());
            }
        }
    }

    // Generate and add the assets for the Phazon Suit
    let (cmdl, ancs) = create_phazon_cmdl_and_ancs(&mut found);
    let key = (cmdl.file_id, cmdl.fourcc());
    if looking_for.remove(&key) {
        assert!(found.insert(key, cmdl).is_none());
    }
    let key = (ancs.file_id, ancs.fourcc());
    if looking_for.remove(&key) {
        assert!(found.insert(key, ancs).is_none());
    }

    assert!(looking_for.is_empty());

    found
}

fn create_phazon_cmdl_and_ancs<'a>(resources: &mut HashMap<(u32, FourCC), structs::Resource<'a>>)
    -> (structs::Resource<'a>, structs::Resource<'a>)
{
    let phazon_suit_cmdl = {
        let grav_suit_cmdl = ResourceData::new(&resources[&(
                asset_ids::GRAVITY_SUIT_CMDL, b"CMDL".into())]);
        let mut phazon_cmdl_bytes = grav_suit_cmdl.decompress().into_owned();

        // Ensure the length is a multiple of 32
        let len = phazon_cmdl_bytes.len();
        phazon_cmdl_bytes.extend(reader_writer::pad_bytes(32, len).iter());

        // Change which textures this points to
        asset_ids::PHAZON_SUIT_TXTR1.write(&mut &mut phazon_cmdl_bytes[0x64..]).unwrap();
        asset_ids::PHAZON_SUIT_TXTR2.write(&mut &mut phazon_cmdl_bytes[0x70..]).unwrap();
        pickup_meta::build_resource(
            asset_ids::PHAZON_SUIT_CMDL,
            structs::ResourceKind::External(phazon_cmdl_bytes, b"CMDL".into())
        )
    };
    let phazon_suit_ancs = {
        let grav_suit_ancs = ResourceData::new(&resources[&(
                asset_ids::GRAVITY_SUIT_ANCS, b"ANCS".into())]);
        let mut phazon_ancs_bytes = grav_suit_ancs.decompress().into_owned();

        // Ensure the length is a multiple of 32
        let len = phazon_ancs_bytes.len();
        phazon_ancs_bytes.extend(reader_writer::pad_bytes(32, len).iter());

        // Change this to refer to the CMDL above
        asset_ids::PHAZON_SUIT_CMDL.write(&mut &mut phazon_ancs_bytes[0x14..]).unwrap();
        pickup_meta::build_resource(
            asset_ids::PHAZON_SUIT_ANCS,
            structs::ResourceKind::External(phazon_ancs_bytes, b"ANCS".into())
        )
    };
    (phazon_suit_cmdl, phazon_suit_ancs)
}

fn artifact_layer_change_template<'a>(instance_id: u32, pickup_kind: u32)
    -> structs::SclyObject<'a>
{
    let layer = if pickup_kind > 29 {
        pickup_kind - 28
    } else {
        assert!(pickup_kind == 29);
        ARTIFACT_OF_TRUTH_REQ_LAYER
    };
    structs::SclyObject {
        instance_id: instance_id,
        connections: vec![].into(),
        property_data: structs::SclyProperty::SpecialFunction(
            structs::SpecialFunction {
                name: Cow::Borrowed(CStr::from_bytes_with_nul(b"Artifact Layer Switch\0").unwrap()),
                position: GenericArray::map_slice(&[0., 0., 0.], Clone::clone),
                rotation: GenericArray::map_slice(&[0., 0., 0.], Clone::clone),
                type_: 16,
                // TODO Working around a compiler bug. Switch this back to being checked later.
                unknown0: Cow::Borrowed(unsafe { CStr::from_bytes_with_nul_unchecked(b"\0") }),
                unknown1: 0.,
                unknown2: 0.,
                unknown3: 0.,
                layer_change_room_id: 3442151074,
                layer_change_layer_id: layer,
                item_id: 0,
                unknown4: 1,
                unknown5: 0.,
                unknown6: 4294967295,
                unknown7: 4294967295,
                unknown8: 4294967295
            }
        ),
    }
}

fn post_pickup_relay_template<'a>(instance_id: u32, connections: &'static [structs::Connection])
    -> structs::SclyObject<'a>
{
    structs::SclyObject {
        instance_id: instance_id,
        connections: connections.to_owned().into(),
        property_data: structs::SclyProperty::Relay(structs::Relay {
            name: Cow::Owned(CString::new(b"Randomizer Post Pickup Relay".to_vec()).unwrap()),
            active: 1,
        })
    }
}

fn skip_hudmemos_strg_id(pickup_meta_idx: usize) -> u32
{
    (asset_ids::SKIP_HUDMEMO_STRG_START..asset_ids::SKIP_HUDMEMO_STRG_END)
        .nth(pickup_meta_idx)
        .unwrap()
}

fn add_skip_hudmemos_strgs(pickup_resources: &mut HashMap<(u32, FourCC), structs::Resource>)
{
    for (pickup_meta_idx, pt) in pickup_meta::pickup_meta_table().iter().enumerate() {
        let id = skip_hudmemos_strg_id(pickup_meta_idx);
        let res = pickup_meta::build_resource(
            id,
            structs::ResourceKind::Strg(structs::Strg {
                string_tables: vec![
                    structs::StrgStringTable {
                        lang: b"ENGL".into(),
                        strings: vec![format!("&just=center;{} acquired!\u{0}",
                                              pt.name).into()].into(),
                    },
                ].into(),
            })
        );
        assert!(pickup_resources.insert((id, b"STRG".into()), res).is_none())
    }
}

fn build_artifact_temple_totem_scan_strings<R>(pickup_layout: &[u8], rng: &mut R)
    -> [String; 12]
    where R: Rng + Rand
{
    let mut generic_text_templates = [
        "I mean, maybe it'll be in the &push;&main-color=#43CD80;{room}&pop;. I forgot, to be honest.\0",
        "I'm not sure where the artifact exactly is, but like, you can try the &push;&main-color=#43CD80;{room}&pop;.\0",
        "Hey man, so some of the Chozo dudes are telling me that they're might be a thing in the &push;&main-color=#43CD80;{room}&pop;. Just sayin'.\0",
        "Uhh umm... Where was it...? Uhhh, errr, it's definitely in the &push;&main-color=#43CD80;{room}&pop;! I am 100% not totally making it up...\0",
        "Some say it may be in the &push;&main-color=#43CD80;{room}&pop;. Others say that you have no business here. Please leave me alone.\0",
        "So a buddy of mine and I were drinking one night and we thought 'Hey, wouldn't be crazy if we put it at the &push;&main-color=#43CD80;{room}&pop;?' So we did and it took both of us just to get it there!\0",
        "So, uhhh, I kind of got a little lazy and I might have just dropped mine somewhere... Maybe it's in the &push;&main-color=#43CD80;{room}&pop;? Who knows.\0",
        "I uhhh... was a little late to the party and someone had to run out and hide both mine and hers. I owe her one. She told me it might be in the &push;&main-color=#43CD80;{room}&pop;, so you're going to have to trust her on this one.\0",
        "Okay, so this jerk forgets to hide his and I had to hide it for him too. So, I just tossed his somewhere and made up a name for the room. This is literally saving the planet - how can anyone forget that? Anyway, mine is in the &push;&main-color=#43CD80;{room}&pop;, so go check it out. I'm never doing this again...\0",
        "To be honest, I don't know if it was a Missile Expansion or not. Maybe it was... We'll just go with that: There's a Missile Expansion at the &push;&main-color=#43CD80;{room}&pop;.\0",
        "Hear the words of Oh Leer, last Chozo of the Artifact Temple. May they serve you well, that you may find a key lost to our cause... Alright, whatever. It's at the &push;&main-color=#43CD80;{room}&pop;.\0",
        "I kind of just played Frisbee with mine. It flew and landed too far so I didn't want to walk over and grab it because I was lazy. It's in the &push;&main-color=#43CD80;{room}&pop; if you want to find it.\0",
    ];
    rng.shuffle(&mut generic_text_templates);
    let mut generic_templates_iter = generic_text_templates.iter();

    // TODO: If there end up being a large number of these, we could use a binary search
    //       instead of searching linearly.
    // XXX It would be nice if we didn't have to use Vec here and could allocated on the stack
    //     instead, but there doesn't seem to be a way to do it that isn't extremely painful or
    //     relies on unsafe code.
    let mut specific_room_templates = [
        // Artifact Temple
        (0x2398E906, vec!["{pickup} awaits those who truly seek it.\0"]),
    ];
    for rt in specific_room_templates.iter_mut() {
        rng.shuffle(&mut rt.1);
    }


    let mut scan_text = [
        String::new(), String::new(), String::new(), String::new(),
        String::new(), String::new(), String::new(), String::new(),
        String::new(), String::new(), String::new(), String::new(),
    ];

    let names_iter = pickup_meta::PICKUP_LOCATIONS.iter()
        .flat_map(|i| i.iter()) // Flatten out the rooms of the paks
        .flat_map(|l| iter::repeat((l.room_id, l.name)).take(l.pickup_locations.len()));
    let iter = pickup_layout.iter()
        .zip(names_iter)
        // ▼▼▼▼ Only yield artifacts ▼▼▼▼
        .filter(|&(pickup_meta_idx, _)| *pickup_meta_idx >= 23 && *pickup_meta_idx <= 34);

    // Shame there isn't a way to flatten tuples automatically
    for (pickup_meta_idx, (room_id, name)) in iter {
        let artifact_id = *pickup_meta_idx as usize - 23;
        if scan_text[artifact_id].len() != 0 {
            // If there are multiple of this particular artifact, then we use the first instance
            // for the location of the artifact.
            continue;
        }

        // If there are specific messages for this room, choose one, other wise choose a generic
        // message.
        let template = specific_room_templates.iter_mut()
            .find(|row| row.0 == room_id)
            .and_then(|row| row.1.pop())
            .unwrap_or_else(|| generic_templates_iter.next().unwrap());
        let pickup_name = pickup_meta::pickup_meta_table()[*pickup_meta_idx as usize].name;
        scan_text[artifact_id] = template.replace("{room}", name).replace("{pickup}", pickup_name);
    }

    // Set a default value for any artifacts that we didn't find.
    for i in 0..scan_text.len() {
        if scan_text[i].len() == 0 {
            scan_text[i] = "Artifact not present. This layout may not be completable.\0".to_owned();
        }
    }
    scan_text
}

fn modify_pickups<R: Rng + Rand>(
    gc_disc: &mut structs::GcDisc,
    pickup_layout: &[u8],
    mut rng: R,
    skip_hudmenus: bool
) {
    let mut pickup_resources = collect_pickup_resources(&gc_disc);
    if skip_hudmenus {
        add_skip_hudmemos_strgs(&mut pickup_resources);
    }

    let artifact_totem_strings = build_artifact_temple_totem_scan_strings(pickup_layout, &mut rng);

    let mut layout_iter = pickup_layout.iter()
        .map(|n| {
            // Use the E-Tank model for a nothing with a 1/4 chance. Otherwise, use the Missile
            // model.
            if *n != 35 { *n as usize } else { 35 + rng.gen_weighted_bool(4) as usize }
        })
        .enumerate();

    let mut fresh_instance_id_range = 0xDEEF0000..;

    for (i, pak_name) in METROID_PAK_NAMES.iter().enumerate() {
        let file_entry = find_file_mut(gc_disc, pak_name);
        file_entry.guess_kind();
        let pak = match *file_entry.file_mut().unwrap() {
            structs::FstEntryFile::Pak(ref mut pak) => pak,
            _ => panic!(),
        };

        modify_pickups_in_pak(
            pak,
            pickup_meta::PICKUP_LOCATIONS[i].iter(),
            &pickup_resources,
            layout_iter.by_ref(),
            &artifact_totem_strings,
            &mut fresh_instance_id_range,
            skip_hudmenus
        );
    }
}

// TODO: It might be nice for this list to be generataed by resource_tracing, but
//       the sorting is probably non-trivial.
const ARTIFACT_TOTEM_SCAN_STRGS: &'static [u32] = &[
    0x61729798,// Lifegiver
    0xAA2E443D,// Wild
    0x8E9C7387,// World
    0x16B057E3,// Sun
    0xB72B7485,// Elder
    0x45C0A022,// Spirit
    0xFAE3D58E,// Truth
    0x2CBA3693,// Chozo
    0xE7E6E536,// Warrior
    0xC354D28C,// Newborn
    0xDDEC8446,// Nature
    0x7C77A720,// Strength
];

fn modify_pickups_in_pak<'a, I, J>(
    pak: &mut structs::Pak<'a>,
    room_list_iter: I,
    pickup_resources: &HashMap<(u32, FourCC), structs::Resource<'a>>,
    layout_iter: &mut J,
    artifact_totem_strings: &[String; 12],
    fresh_instance_id_range: &mut RangeFrom<u32>,
    skip_hudmenus: bool,
)
    where I: Iterator<Item = &'static pickup_meta::RoomInfo>,
          J: Iterator<Item = (usize, usize)>,
{
    let resources = &mut pak.resources;

    // To appease the borrow checker, make a copy of the Mlvl on the stack that
    // we'll update as we go. When we're done manipulating all the other resources
    // in the pak, we'll write this copy over the one in the pak.
    let mlvl = resources.iter()
        .find(|i| i.fourcc() == reader_writer::FourCC::from_bytes(b"MLVL"))
        .unwrap().kind.as_mlvl().unwrap().into_owned();

    let mut editor = mlvl_wrapper::MlvlEditor::new(mlvl);

    let mut room_list_iter = room_list_iter.peekable();

    let mut cursor = resources.cursor();
    loop {
        let mut cursor = cursor.cursor_advancer();

        match cursor.peek().map(|res| (res.file_id, res.fourcc())) {
            None => panic!("Unexpectedly reached the end of the pak"),
            Some((_, fourcc)) if fourcc == b"MLVL".into() => {
                // Update the Mlvl in the table with version we've been updating
                let mut res = cursor.value().unwrap().kind.as_mlvl_mut().unwrap();
                *res = editor.mlvl;
                // The Mlvl is the last entry in the PAK, so break here.
                break;
            },
            Some((asset_ids::PHAZON_MINES_SAVW, fourcc)) if fourcc == b"SAVW".into() => {
                // Add a scan for the Phazon suit.
                let mut savw = cursor.value().unwrap().kind.as_savw_mut().unwrap();
                savw.scan_array.as_mut_vec().push(structs::ScannableObject {
                    scan: asset_ids::PHAZON_SUIT_SCAN,
                    logbook_category: 0,
                });
            },
            Some((file_id, fourcc)) if fourcc == b"STRG".into() => {
                if let Some(pos) = ARTIFACT_TOTEM_SCAN_STRGS.iter().position(|id| *id == file_id) {
                    // Replace the text of the scans of the totems in the Artifact Temple
                    let mut strg = cursor.value().unwrap().kind.as_strg_mut().unwrap();
                    for st in strg.string_tables.as_mut_vec().iter_mut() {
                        let strings = st.strings.as_mut_vec();
                        *strings.last_mut().unwrap() = artifact_totem_strings[pos].clone().into();
                    }
                }
            },
            Some((file_id, fourcc)) if fourcc == b"MREA".into() => {
                if let Some(&&room_info) = room_list_iter.peek() {
                    if room_info.room_id == file_id {
                        room_list_iter.next();
                        let area = editor.get_area(&mut cursor);
                        modify_pickups_in_mrea(
                            area,
                            room_info,
                            pickup_resources,
                            layout_iter,
                            fresh_instance_id_range,
                            skip_hudmenus
                        );
                    }
                }
            }
            _ => (),
        };

    }
}

fn modify_pickups_in_mrea<'a, 'mlvl, 'cursor, 'list, I>(
    mut area: mlvl_wrapper::MlvlArea<'a, 'mlvl, 'cursor, 'list>,
    room_info: pickup_meta::RoomInfo,
    pickup_resources: &HashMap<(u32, FourCC), structs::Resource<'a>>,
    layout_iter: &mut I,
    fresh_instance_id_range: &mut RangeFrom<u32>,
    skip_hudmenus: bool,
)
    where I: Iterator<Item = (usize, usize)>,
{
    // Remove objects
    {
        let scly = area.mrea().scly_section_mut();
        let layers = scly.layers.as_mut_vec();
        for otr in room_info.objects_to_remove {
            layers[otr.layer as usize].objects.as_mut_vec()
                .retain(|i| !otr.instance_ids.contains(&i.instance_id));
        }
    }

    for &pickup_location in room_info.pickup_locations {

        let (location_idx, pickup_meta_idx) = layout_iter.next().unwrap();
        let pickup_meta = &pickup_meta::pickup_meta_table()[pickup_meta_idx];
        let iter = pickup_meta.deps.iter().map(|&(file_id, fourcc)| structs::Dependency {
                asset_id: file_id,
                asset_type: fourcc,
            });

        // TODO: Re-randomization: reuse the same layer, just clear its contents
        //       (and remove any connections to any objects contained within it)
        let name = CString::new(format!(
                "Randomizer - Pickup {} ({:?})", location_idx, pickup_meta.pickup.name)).unwrap();
        area.add_layer(name);

        let new_layer_idx = area.layer_flags.layer_count as usize - 1;
        if !skip_hudmenus {
            area.add_dependencies(pickup_resources, new_layer_idx, iter);
        } else {
            // Add our custom STRG
            let iter = iter.chain(iter::once(structs::Dependency {
                    asset_id: skip_hudmemos_strg_id(pickup_meta_idx),
                    asset_type: b"STRG".into(),
                }));
            area.add_dependencies(pickup_resources, new_layer_idx, iter);
        }

        if area.mrea_file_id() == asset_ids::ARTIFACT_TEMPLE_MREA {
            // If this room is the Artifact Temple, patch it.
            assert_eq!(room_info.pickup_locations.len(), 1, "Sanity check");
            fix_artifact_of_truth_requirement(&mut area, pickup_meta.pickup.kind,
                                                fresh_instance_id_range);
        }

        let scly = area.mrea().scly_section_mut();
        let layers = scly.layers.as_mut_vec();

        let mut additional_connections = Vec::new();

        // Add a post-pickup relay. This is used to support cutscene-skipping
        let instance_id = fresh_instance_id_range.next().unwrap();
        let relay = post_pickup_relay_template(instance_id,
                                                pickup_location.post_pickup_relay_connections);
        layers[new_layer_idx].objects.as_mut_vec().push(relay);
        additional_connections.push(structs::Connection {
            state: 1,
            message: 13,
            target_object_id: instance_id,
        });

        // If this is an artifact, insert a layer change function
        let pickup_kind = pickup_meta.pickup.kind;
        if pickup_kind >= 29 && pickup_kind <= 40 {
            let instance_id = fresh_instance_id_range.next().unwrap();
            let function = artifact_layer_change_template(instance_id, pickup_kind);
            layers[new_layer_idx].objects.as_mut_vec().push(function);
            additional_connections.push(structs::Connection {
                state: 1,
                message: 7,
                target_object_id: instance_id,
            });
        }

        {
            let pickup = layers[pickup_location.location.layer as usize].objects.iter_mut()
                .find(|obj| obj.instance_id ==  pickup_location.location.instance_id)
                .unwrap();
            update_pickup(pickup, &pickup_meta);
            if additional_connections.len() > 0 {
                pickup.connections.as_mut_vec().extend_from_slice(&additional_connections);
            }
        }
        {
            let hudmemo = layers[pickup_location.hudmemo.layer as usize].objects.iter_mut()
                .find(|obj| obj.instance_id ==  pickup_location.hudmemo.instance_id)
                .unwrap();
            update_hudmemo(hudmemo, pickup_meta_idx, &pickup_meta, location_idx, skip_hudmenus);
        }
        {
            let location = pickup_location.attainment_audio;
            let attainment_audio = layers[location.layer as usize].objects.iter_mut()
                .find(|obj| obj.instance_id ==  location.instance_id)
                .unwrap();
            update_attainment_audio(attainment_audio, &pickup_meta);
        }
    }
}

fn update_pickup(pickup: &mut structs::SclyObject, pickup_meta: &pickup_meta::PickupMeta)
{
    let pickup = pickup.property_data.as_pickup_mut().unwrap();
    let original_pickup = pickup.clone();

    let original_aabb = pickup_meta::aabb_for_pickup_cmdl(original_pickup.cmdl).unwrap();
    let new_aabb = pickup_meta::aabb_for_pickup_cmdl(pickup_meta.pickup.cmdl).unwrap();
    let original_center = calculate_center(original_aabb, original_pickup.rotation,
                                            original_pickup.scale);
    let new_center = calculate_center(new_aabb, pickup_meta.pickup.rotation,
                                        pickup_meta.pickup.scale);

    // The pickup needs to be repositioned so that the center of its model
    // matches the center of the original.
    *pickup = structs::Pickup {
        position: GenericArray::from_slice(&[
            original_pickup.position[0] - (new_center[0] - original_center[0]),
            original_pickup.position[1] - (new_center[1] - original_center[1]),
            original_pickup.position[2] - (new_center[2] - original_center[2]),
        ]),
        hitbox: original_pickup.hitbox,
        scan_offset: GenericArray::from_slice(&[
            original_pickup.scan_offset[0] + (new_center[0] - original_center[0]),
            original_pickup.scan_offset[1] + (new_center[1] - original_center[1]),
            original_pickup.scan_offset[2] + (new_center[2] - original_center[2]),
        ]),

        fade_in_timer: original_pickup.fade_in_timer,
        unknown: original_pickup.unknown,
        active: original_pickup.active,

        ..(pickup_meta.pickup.clone())
    };
}

fn update_hudmemo(
    hudmemo: &mut structs::SclyObject,
    pickup_meta_idx: usize,
    pickup_meta: &pickup_meta::PickupMeta,
    location_idx: usize,
    skip_hudmenus: bool)
{
    // The items in Watery Hall (Charge beam), Research Core (Thermal Visor), and Artifact Temple
    // (Artifact of Truth) should always have modal hudmenus to because a cutscene plays
    // immediately after each item is acquired, and the nonmodal hudmenu wouldn't properly appear.
    const ALWAYS_MODAL_HUDMENUS: &[usize] = &[23, 50, 63];
    let hudmemo = hudmemo.property_data.as_hud_memo_mut().unwrap();
    if skip_hudmenus && !ALWAYS_MODAL_HUDMENUS.contains(&location_idx) {
        hudmemo.first_message_timer = 5.;
        hudmemo.memo_type = 0;
        hudmemo.strg = skip_hudmemos_strg_id(pickup_meta_idx);
    } else {
        hudmemo.strg = pickup_meta.hudmemo_strg;
    }
}

fn update_attainment_audio(attainment_audio: &mut structs::SclyObject,
                           pickup_meta: &pickup_meta::PickupMeta)
{
    let attainment_audio = attainment_audio.property_data.as_streamed_audio_mut().unwrap();
    let bytes = pickup_meta.attainment_audio_file_name.as_bytes();
    attainment_audio.audio_file_name = Cow::Borrowed(CStr::from_bytes_with_nul(bytes).unwrap());
}

fn calculate_center(aabb: [f32; 6], rotation: GenericArray<f32, U3>, scale: GenericArray<f32, U3>)
    -> [f32; 3]
{
    let start = [aabb[0], aabb[1], aabb[2]];
    let end = [aabb[3], aabb[4], aabb[5]];

    let mut position = [0.; 3];
    for i in 0..3 {
        position[i] = (start[i] + end[i]) / 2. * scale[i];
    }

    rotate(position, [rotation[0], rotation[1], rotation[2]], [0.; 3])
}

fn rotate(mut coordinate: [f32; 3], mut rotation: [f32; 3], center: [f32; 3])
    -> [f32; 3]
{
    // Shift to the origin
    for i in 0..3 {
        coordinate[i] -= center[i];
        rotation[i] = rotation[i].to_radians();
    }

    for i in 0..3 {
        let original = coordinate.clone();
        let x = (i + 1) % 3;
        let y = (i + 2) % 3;
        coordinate[x] = original[x] * rotation[i].cos() - original[y] * rotation[i].sin();
        coordinate[y] = original[x] * rotation[i].sin() + original[y] * rotation[i].cos();
    }

    // Shift back to original position
    for i in 0..3 {
        coordinate[i] += center[i];
    }
    coordinate
}

fn parse_pickup_layout(text: &str) -> Result<(Vec<u8>, [u32; 4]), String>
{
    const LAYOUT_CHAR_TABLE: [u8; 64] =
        *b"ABCDEFGHIJKLMNOPQRSTUWVXYZabcdefghijklmnopqrstuwvxyz0123456789-_";

    if !text.is_ascii() {
        return Err("Pickup layout string contains non-ascii characters.".to_string());
    }
    let text = text.as_bytes();
    if text.len() != 87 {
        return Err("Pickup layout should be exactly 87 characters".to_string());
    }

    let mut sum: BigUint = 0u8.into();
    let mut res = vec![];
    for c in text.iter().rev() {
        if let Some(idx) = LAYOUT_CHAR_TABLE.iter().position(|i| i == c) {
            sum = sum * BigUint::from(64u8) + BigUint::from(idx);
        } else {
            return Err(format!("Pickup layout contains invalid character '{}'.", c));
        }
    }

    let sum_bytes = sum.to_bytes_le();
    let seed = [
        sum_bytes[0..17].iter().fold(0u32, |n, i| n.overflowing_add(*i as u32).0),
        sum_bytes[17..34].iter().fold(0u32, |n, i| n.overflowing_add(*i as u32).0),
        sum_bytes[34..41].iter().fold(0u32, |n, i| n.overflowing_add(*i as u32).0),
        sum_bytes[41..].iter().fold(0u32, |n, i| n.overflowing_add(*i as u32).0),
    ];

    // Reverse the order of the odd bits
    let mut bits = sum.to_str_radix(2).into_bytes();
    for i in 0..(bits.len() / 4) {
        let len = bits.len() - bits.len() % 2;
        bits.swap(i * 2 + 1, len - i * 2 - 1);
    }
    sum = BigUint::parse_bytes(&bits, 2).unwrap();

    // The upper 5 bits are a checksum, so seperate them from the sum.
    let checksum_bitmask = BigUint::from(0b11111u8) << 517;
    let checksum = sum.clone() & checksum_bitmask;
    sum = sum - checksum.clone();
    let checksum = (checksum >> 517).to_u8().unwrap();

    let mut computed_checksum = 0;
    {
        let mut sum = sum.clone();
        while sum > 0u8.into() {
            let remainder = (sum.clone() & BigUint::from(0b11111u8)).to_u8().unwrap();
            computed_checksum = (computed_checksum + remainder) % 32;
            sum = sum >> 5;
        }
    }
    if checksum != computed_checksum {
        return Err("Pickup layout checksum failed.".to_string());
    }

    for _ in 0..100 {
        let (quotient, remainder) = sum.div_rem(&36u8.into());
        res.push(remainder.to_u8().unwrap());
        sum = quotient;
    }

    assert!(sum == 0u8.into());

    res.reverse();
    Ok((res, seed))
}

// Patches the current room to make the Artifact of Truth required to complete
// the game. This logic is based on the observed behavior of Claris's randomizer.
// XXX I still don't entirely understand why there needs to be a special case
//     if an artifact is placed in this room.
fn fix_artifact_of_truth_requirement(area: &mut mlvl_wrapper::MlvlArea,
                                     pickup_kind: u32,
                                     fresh_instance_id_range: &mut RangeFrom<u32>)
{
    let truth_req_layer_id = area.layer_flags.layer_count;
    assert_eq!(truth_req_layer_id, ARTIFACT_OF_TRUTH_REQ_LAYER);

    // Create a new layer that will be toggled on when the Artifact of Truth is collected
    area.add_layer(CString::new("Randomizer - Got Artifact 1".to_string()).unwrap());

    // TODO: Manually verify the correct layers are being toggled
    if pickup_kind != 29 {
        // If the item in the Artifact Temple isn't the artifact of truth, mark
        // the new layer inactive. Note, the layer is active when created.
        area.layer_flags.flags &= !(1 << truth_req_layer_id);
    }
    if pickup_kind >= 30 && pickup_kind <= 40 {
        // If the item in the Artfact Temple is an artifact (other than Truth)
        // mark its layer as active.
        area.layer_flags.flags |= 1 << (pickup_kind - 28);
    }
    // TODO: Re-randomizing: the other Got Artifact layers need to be marked inactive

    let scly = area.mrea().scly_section_mut();

    // A relay is created and connected to "Relay Show Progress 1"
    let new_relay_instance_id = fresh_instance_id_range.next().unwrap();
    let new_relay = structs::SclyObject {
        instance_id: new_relay_instance_id,
        connections: vec![
            structs::Connection {
                state: 9,
                message: 13,
                target_object_id: 1048869,
            },
        ].into(),
        property_data: structs::SclyProperty::Relay(structs::Relay {
            name: Cow::Borrowed(CStr::from_bytes_with_nul(b"Relay Show Progress1\0").unwrap()),
            active: 1,
        }),
    };
    scly.layers.as_mut_vec()[truth_req_layer_id as usize].objects.as_mut_vec().push(new_relay);

    // An existing relay is disconnected from "Relay Show Progress 1" and connected
    // to the new relay
    let relay = scly.layers.as_mut_vec()[1].objects.iter_mut()
        .find(|i| i.instance_id == 68158836).unwrap();
    relay.connections.as_mut_vec().retain(|i| i.target_object_id != 1048869);
    relay.connections.as_mut_vec().push(structs::Connection {
        state: 9,
        message: 13,
        target_object_id: new_relay_instance_id,
    });
}

fn patch_starting_pickups<'a>(gc_disc: &mut structs::GcDisc<'a>, mut starting_items: u64)
{
    let file_entry = find_file_mut(gc_disc, "Metroid4.pak");
    file_entry.guess_kind();
    let pak = match *file_entry.file_mut().unwrap() {
        structs::FstEntryFile::Pak(ref mut pak) => pak,
        _ => panic!(),
    };


    // Find the first MREA in the pak
    let mut cursor = pak.resources.cursor();
    loop {
        if cursor.peek().unwrap().fourcc() == b"MREA".into() {
            break;
        }
        cursor.next();
    }
    let mrea = cursor.value().unwrap().kind.as_mrea_mut().unwrap();
    let scly = mrea.scly_section_mut();

    let mut fetch_bits = |bits: u8| {
        let ret = starting_items & ((1 << bits) - 1);
        starting_items = starting_items >> bits;
        ret as u32
    };

    // The object we want is in the first layer.
    let ref mut layer = scly.layers.as_mut_vec()[0];
    let obj = layer.objects.iter_mut().find(|obj| obj.property_data.is_spawn_point()).unwrap();
    let spawn_point = obj.property_data.as_spawn_point_mut().unwrap();

    println!("Starting pickups set:");

    spawn_point.missiles = fetch_bits(8);
    println!("    missiles: {}", spawn_point.missiles);

    spawn_point.energy_tanks = fetch_bits(4);
    println!("    energy_tanks: {}", spawn_point.energy_tanks);

    spawn_point.power_bombs = fetch_bits(3);
    println!("    power_bombs: {}", spawn_point.power_bombs);

    spawn_point.wave = fetch_bits(1);
    println!("    wave: {}", spawn_point.wave);

    spawn_point.ice = fetch_bits(1);
    println!("    ice: {}", spawn_point.ice);

    spawn_point.plasma = fetch_bits(1);
    println!("    plasma: {}", spawn_point.plasma);

    spawn_point.charge = fetch_bits(1);
    println!("    charge: {}", spawn_point.plasma);

    spawn_point.morph_ball = fetch_bits(1);
    println!("    morph_ball: {}", spawn_point.morph_ball);

    spawn_point.bombs = fetch_bits(1);
    println!("    bombs: {}", spawn_point.bombs);

    spawn_point.spider_ball = fetch_bits(1);
    println!("    spider_ball: {}", spawn_point.spider_ball);

    spawn_point.boost_ball = fetch_bits(1);
    println!("    boost_ball: {}", spawn_point.boost_ball);

    spawn_point.gravity_suit = fetch_bits(1);
    println!("    gravity_suit: {}", spawn_point.gravity_suit);

    spawn_point.phazon_suit = fetch_bits(1);
    println!("    phazon_suit: {}", spawn_point.phazon_suit);

    spawn_point.thermal_visor = fetch_bits(1);
    println!("    thermal_visor: {}", spawn_point.thermal_visor);

    spawn_point.xray= fetch_bits(1);
    println!("    xray: {}", spawn_point.xray);

    spawn_point.space_jump = fetch_bits(1);
    println!("    space_jump: {}", spawn_point.space_jump);

    spawn_point.grapple = fetch_bits(1);
    println!("    grapple: {}", spawn_point.grapple);

    spawn_point.super_missile = fetch_bits(1);
    println!("    super_missile: {}", spawn_point.super_missile);

}

fn patch_dol_skip_frigate<'a>(gc_disc: &mut structs::GcDisc<'a>)
{
    let dol = find_file_mut(gc_disc, "default.dol");
    let file = dol.file_mut().unwrap();
    let reader = match file {
        &mut structs::FstEntryFile::Unknown(ref reader) => reader.clone(),
        _ => panic!(),
    };

    // Replace 4 of the bytes in the main dol. By using chain() like this, we
    // can avoid copying the contents of the dol onto the heap.
    static REPLACEMENT_1: &'static [u8] = &[0x39, 0xF3];
    static REPLACEMENT_2: &'static [u8] = &[0xDE, 0x28];
    let data = reader[..0x1FF1E]
        .chain(REPLACEMENT_1)
        .chain(&reader[0x1FF20..0x1FF2A])
        .chain(REPLACEMENT_2)
        .chain(&reader[0x1FF2C..]);
    *file = structs::FstEntryFile::ExternalFile(structs::ReadWrapper::new(data), reader.len());
}

const FMV_NAMES: &'static [&'static [u8]] = &[
    b"attract0.thp",
    b"attract1.thp",
    b"attract2.thp",
    b"attract3.thp",
    b"attract4.thp",
    b"attract5.thp",
    b"attract6.thp",
    b"attract7.thp",
    b"attract8.thp",
    b"attract9.thp",
];
fn replace_fmvs(gc_disc: &mut structs::GcDisc)
{
    const FMV: &'static [u8] = include_bytes!("../../extra_assets/attract_mode.thp");
    let fst = &mut gc_disc.file_system_table;
    let fmv_entries = fst.fst_entries.iter_mut()
        .filter(|e| FMV_NAMES.contains(&e.name.to_bytes()));
    for entry in fmv_entries {
        let file = entry.file_mut().unwrap();
        let rw = structs::ReadWrapper::new(FMV);
        *file = structs::FstEntryFile::ExternalFile(rw, FMV.len());
    }
}


struct ProgressNotifier
{
    total_size: usize,
    bytes_so_far: usize,
    quiet: bool,
}

impl ProgressNotifier
{
    fn new(quiet: bool) -> ProgressNotifier
    {
        ProgressNotifier {
            total_size: 0,
            bytes_so_far: 0,
            quiet: quiet,
        }
    }

    fn notify_flushing_to_disk(&mut self)
    {
        if self.quiet {
            return;
        }
        println!("Flushing written data to the disk...");
    }
}

impl structs::ProgressNotifier for ProgressNotifier
{
    fn notify_total_bytes(&mut self, total_size: usize)
    {
        self.total_size = total_size
    }

    fn notify_writing_file(&mut self, file_name: &reader_writer::CStr, file_bytes: usize)
    {
        if self.quiet {
            return;
        }
        let percent = self.bytes_so_far as f64 / self.total_size as f64 * 100.;
        println!("{:02.0}% -- Writing file {:?}", percent, file_name);
        self.bytes_so_far += file_bytes;
    }

    fn notify_writing_header(&mut self)
    {
        if self.quiet {
            return;
        }
        let percent = self.bytes_so_far as f64 / self.total_size as f64 * 100.;
        println!("{:02.0}% -- Writing ISO header", percent);
    }
}

fn main_inner() -> Result<(), String>
{
    pickup_meta::setup_pickup_meta_table();

    let matches = App::new("randomprime ISO patcher")
        .version(crate_version!())
        .arg(Arg::with_name("input iso path")
            .long("input-iso")
            .required(true)
            .takes_value(true))
        .arg(Arg::with_name("output iso path")
            .long("output-iso")
            .required(true)
            .takes_value(true))
        .arg(Arg::with_name("pickup layout")
            .long("layout")
            .required(true)
            .takes_value(true))
        .arg(Arg::with_name("skip frigate")
            .long("skip-frigate")
            .help("New save files will skip the \"Space Pirate Frigate\" tutorial level"))
        .arg(Arg::with_name("skip hudmenus")
            .long("non-modal-item-messages")
            .help("Display a non-modal message when an item is is acquired"))
        .arg(Arg::with_name("keep attract mode")
            .long("keep-attract-mode")
            .help("Keeps the attract mode FMVs, which are removed by default"))
        .arg(Arg::with_name("quiet")
            .long("quiet")
            .help("Don't print the progress messages"))
        .arg(Arg::with_name("change starting items")
            .long("starting-items")
            .hidden(true)
            .takes_value(true)
            .validator(|s| s.parse::<u64>().map(|_| ())
                                           .map_err(|_| "Expected an integer".to_string())))
        .get_matches();

    let input_iso_path = matches.value_of("input iso path").unwrap();
    let output_iso_path = matches.value_of("output iso path").unwrap();
    let pickup_layout = matches.value_of("pickup layout").unwrap();
    let skip_hudmenus = matches.is_present("skip hudmenus");
    let skip_frigate = matches.is_present("skip frigate");
    let keep_fmvs = matches.is_present("keep attract mode");
    let quiet = matches.is_present("quiet");
    let starting_items = matches.value_of("change starting items");

    let (pickup_layout, seed) = parse_pickup_layout(pickup_layout)?;
    assert_eq!(pickup_layout.len(), 100);

    let file = File::open(input_iso_path)
                .map_err(|e| format!("Failed to open input iso: {}", e))?;
    let mmap = memmap::Mmap::open(&file, memmap::Protection::Read)
                .map_err(|e| format!("Failed to open input iso: {}", e))?;
    let mut reader = Reader::new(unsafe { mmap.as_slice() });

    // On non-debug builds, suppress the default panic message and print a more helpful and
    // user-friendly one
    if !cfg!(debug_assertions) {
        panic::set_hook(Box::new(|_| {
            let _ = writeln!(io::stderr(), "{} \
An error occurred while parsing the input ISO. \
This most likely means your ISO is corrupt. \
Please verify that your ISO matches one of the following hashes:
MD5:  737cbfe7230af3df047323a3185d7e57
SHA1: 1c8b27af7eed2d52e7f038ae41bb682c4f9d09b5
", Format::Error("error:"));
        }));
    }

    let mut gc_disc: structs::GcDisc = reader.read(());

    if &gc_disc.header.game_identifier() != b"GM8E01" {
        Err("The input ISO doesn't appear to be Metroid Prime.".to_string())?
    }

    let rng = XorShiftRng::from_seed(seed);
    modify_pickups(&mut gc_disc, &pickup_layout, rng, skip_hudmenus);

    if skip_frigate {
        patch_dol_skip_frigate(&mut gc_disc);

        // To reduce the amount of data that needs to be copied, empty the contents of the pak
        let file_entry = find_file_mut(&mut gc_disc, "Metroid1.pak");
        file_entry.guess_kind();
        match file_entry.file_mut() {
            Some(&mut structs::FstEntryFile::Pak(ref mut pak)) => pak.resources.clear(),
            _ => (),
        };
    }

    if let Some(starting_items) = starting_items.map(|s| s.parse::<u64>().unwrap()) {
        patch_starting_pickups(&mut gc_disc, starting_items);
    }

    if !keep_fmvs {
        replace_fmvs(&mut gc_disc);
    }

    let pn = ProgressNotifier::new(quiet);
    write_gc_disc(&mut gc_disc, output_iso_path, pn)?;
    println!("Done");
    Ok(())
}

fn main()
{
    let _ = match main_inner() {
        Err(s) => writeln!(io::stderr(), "{} {}", Format::Error("error:"), s),
        Ok(()) => Ok(()),
    };
}