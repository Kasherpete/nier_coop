//! Reversed engine types and patterns for the current Steam build (post-2021 "Become as Gods" exe).
//!
//! Source: praydog/AutomataMP (MIT), `reversing/nierautomata.genny` and `shared/sdk/*`, last updated
//! 2024-01-20 against the Steam build, plus this project's own reversing (exe built 2026-10-09).

use std::mem::offset_of;

use bytemuck::{Pod, Zeroable};

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable)]
pub struct cVec4 {
    pub x: f32, // 0x00
    pub y: f32, // 0x04
    pub z: f32, // 0x08
    pub w: f32, // 0x0C
}
const _: () = assert!(size_of::<cVec4>() == 0x10);

/// Playable character (2B / 9S / A2). Inherits Model -> Obj -> Behavior -> BehaviorAppBase.
#[repr(C)]
pub struct Pl0000 {
    pub vtable: *const usize,  // 0x00000
    _pad_00008: [u8; 0x48],    // 0x00008
    pub position: cVec4,       // 0x00050 Model::position
    _pad_00060: [u8; 0x30],    // 0x00060
    pub rotation: cVec4,       // 0x00090 Model rotation; .y (0x94) is Model::facing (yaw)
    _pad_000a0: [u8; 0x4F8],   // 0x000A0
    pub obj_flags: u32,        // 0x00598 Obj::obj_flags
    _pad_0059c: [u8; 0x2BC],   // 0x0059C
    pub health: u32,           // 0x00858 BehaviorAppBase::health
    _pad_0085c: [u8; 0x584],   // 0x0085C
    pub ai_pod_fire: i32,      // 0x00DE0 > 0: pod fires; read instead of held_flags bit 6 when not player-controlled (vtable +0x350 == 0)
    _pad_00de4: [u8; 0x5F4],   // 0x00DE4
    pub held_flags: u32,       // 0x013D8 CharacterController (@0xCA0) ::held_flags (@0x738), bit i = buttons[i] held
    _pad_013dc: [u8; 0x34],    // 0x013DC
    pub buttons: [u32; 8],     // 0x01410 CharacterController::buttons (@0x770): light, heavy, jump, ?, pod special, ?, pod fire, dash
    _pad_01430: [u8; 0x4],     // 0x01430
    pub speed: f32,            // 0x01434 CharacterController::speed (@0x794), stick magnitude
    _pad_01438: [u8; 0x4],     // 0x01438
    pub facing2: f32,          // 0x0143C CharacterController::facing (@0x79C), stick direction
    _pad_01440: [u8; 0xEF24],  // 0x01440
    pub muteki_timer: f32,     // 0x10364 invincible while > 0; script Pl0000.setEventMuteki sets 300.0
    _pad_10368: [u8; 0x2498],  // 0x10368 (0x10B64: vtable +0x350's player-controlled flag, 0 for the remote)
    pub pod_aim: cVec4,        // 0x12800 world point pod_shot aims at (camera ray for the player, pod_target for others)
    _pad_12810: [u8; 0x3C6C],  // 0x12810
    pub buddy_handle: u32,     // 0x1647C
    _pad_16480: [u8; 0x8],     // 0x16480
    pub run_speed_type: u32,   // 0x16488 0 = player (speed from controller), 1 = buddy (speed from AI)
    _pad_1648c: [u8; 0x50],    // 0x1648C
    pub pod_target: u32,       // 0x164DC entity handle a non-player-controlled pod shoots at; 0 = pod_shot fires nothing
    _pad_164e0: [u8; 0x550],   // 0x164E0
    pub pod_cooldown: [[f32; 2]; 3], // 0x16A30 per pod index: [full, remaining]; a program starts only at remaining <= 0
    _pad_16a48: [u8; 0xED8],   // 0x16A48
}
const _: () = {
    assert!(size_of::<Pl0000>() == 0x17920);
    assert!(offset_of!(Pl0000, position) == 0x50);
    assert!(offset_of!(Pl0000, rotation) == 0x90);
    assert!(offset_of!(Pl0000, obj_flags) == 0x598);
    assert!(offset_of!(Pl0000, health) == 0x858);
    assert!(offset_of!(Pl0000, ai_pod_fire) == 0xDE0);
    assert!(offset_of!(Pl0000, held_flags) == 0x13D8);
    assert!(offset_of!(Pl0000, buttons) == 0x1410);
    assert!(offset_of!(Pl0000, speed) == 0x1434);
    assert!(offset_of!(Pl0000, facing2) == 0x143C);
    assert!(offset_of!(Pl0000, muteki_timer) == 0x10364);
    assert!(offset_of!(Pl0000, pod_aim) == 0x12800);
    assert!(offset_of!(Pl0000, buddy_handle) == 0x1647C);
    assert!(offset_of!(Pl0000, pod_target) == 0x164DC);
    assert!(offset_of!(Pl0000, pod_cooldown) == 0x16A30);
    assert!(offset_of!(Pl0000, run_speed_type) == 0x16488);
};

/// Offset of `CharacterController` inside `Pl0000`; code inside the controller gets `this = pl + 0xCA0`.
pub const CONTROLLER_OFFSET: usize = 0xCA0;
/// `held_flags` bit of the pod fire button (`buttons[6]`).
pub const HELD_POD_FIRE: u32 = 1 << 6;
/// `Pl0000` u32 fields that must all be 0 before the engine runs the pod-program update (gates in 0x528c70).
pub const POD_PROGRAM_BLOCKERS: [usize; 5] = [0x17678, 0x12A68, 0x12AA4, 0x165FC, 0x12AEC];
/// `Pl0000` flags field whose bit 0x20 also blocks it.
pub const POD_PROGRAM_FLAGS: usize = 0x16D18;
/// `Pl0000` u32 index of the active pod (into the pod handles at 0x10810 and `pod_cooldown`).
pub const POD_INDEX: usize = 0x10820;
/// `Pl0000` state id; states 0x41 and 0x46 block it.
pub const PL_STATE: usize = 0x670;
/// `EModel` of 2B.
pub const MODEL_2B: u32 = 0x10000;
/// `start_animation` ids that crash the game when started on another character (AutomataMP `EAnimation`).
pub const ANIM_CRASHES: [u32; 4] = [27, 65, 104, 106];
/// `start_animation` ids the synced buttons already cause (light attack, dash); AutomataMP replays all others,
/// including walk/run/sprint, which the engine does not start from `speed` alone on a spawned partner.
pub const ANIM_FROM_INPUT: [u32; 2] = [16, 18];
/// Animation the spawned partner's AI starts every idle frame (seen in-game). While in it, its pod never spawns
/// shots, so the remote's own calls with it are dropped and it stays in idle (0) like the player it mirrors.
pub const ANIM_PARTNER_WAIT: u32 = 1_000_000;

/// Transform passed to the spawn function.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SpawnMatrix {
    pub forward: cVec4,  // 0x00
    pub up: cVec4,       // 0x10
    pub right: cVec4,    // 0x20
    pub w: cVec4,        // 0x30
    pub position: cVec4, // 0x40
    pub rotation: cVec4, // 0x50 copied from a Pl0000's `rotation`
    pub scale: cVec4,    // 0x60
    pub unk: [u32; 8],   // 0x70 unk[6] (0x88) = 1
}
const _: () = assert!(size_of::<SpawnMatrix>() == 0x90);

/// Argument of the entity spawn function (`EntitySpawnParams` in AutomataMP).
#[repr(C)]
pub struct SpawnParams {
    pub name: *const u8,            // 0x00 NUL-terminated entity name
    pub model: u32,                 // 0x08 EModel
    pub model2: u32,                // 0x0C same as model
    pub matrix: *const SpawnMatrix, // 0x10
    pub unk1: u64,                  // 0x18
    pub unk2: u32,                  // 0x20 = 1
    _pad_24: u32,                   // 0x24
    pub unk3: u64,                  // 0x28
    pub unk4: cVec4,                // 0x30
    pub unk5: cVec4,                // 0x40
    pub unk6: u64,                  // 0x50
    _pad_58: [u8; 0x28],            // 0x58
}
const _: () = assert!(size_of::<SpawnParams>() == 0x80 && offset_of!(SpawnParams, unk6) == 0x50);

impl SpawnParams {
    pub fn new(name: &'static [u8], model: u32, matrix: &SpawnMatrix) -> Self {
        assert!(name.ends_with(b"\0"));
        let z = cVec4::default();
        Self { name: name.as_ptr(), model, model2: model, matrix, unk1: 0, unk2: 1, _pad_24: 0, unk3: 0, unk4: z, unk5: z, unk6: 0, _pad_58: [0; 0x28] }
    }
}

/// Node of the engine's circular list of script-callable functions ("Class.method" -> native).
#[repr(C)]
pub struct ScriptFunction {
    pub vtable: usize,             // 0x00
    pub crc: u32,                  // 0x08 CRC-32 of "Class.method"
    _pad_0c: u32,                  // 0x0C
    pub next: *const ScriptFunction, // 0x10
    _unk_18: usize,                // 0x18
    pub function: usize,           // 0x20
}
const _: () = assert!(size_of::<ScriptFunction>() == 0x28 && offset_of!(ScriptFunction, function) == 0x20);

/// Vtable slot of `Behavior::start_animation(this, anim, variant, a3, a4)`.
pub const START_ANIMATION_INDEX: usize = 18;

/// Entity container; `behavior` points at the `Behavior` subclass (a `Pl0000` only after a vtable check).
#[repr(C)]
pub struct Entity {
    _unknown: usize,           // 0x00
    pub name: [u8; 0x20],      // 0x08 NUL-terminated
    pub object_id: u32,        // 0x28 type id: 0x1xxxx pl, 0x2xxxx em (enemy), 0x3xxxx wp, ...
    _flags: u32,               // 0x2C
    pub handle: u32,           // 0x30
    _pad_34: u32,              // 0x34
    _unknown2: usize,          // 0x38
    _unknown3: usize,          // 0x40
    pub behavior: *mut Pl0000, // 0x48
}
const _: () = assert!(size_of::<Entity>() == 0x50 && offset_of!(Entity, behavior) == 0x48);

/// One slot of the global entity array, indexed by `(handle >> 8) & 0xFFFF`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EntityDescriptor {
    pub handle: u32,      // 0x00
    _pad_04: u32,         // 0x04
    pub ent: *mut Entity, // 0x08
}
const _: () = assert!(size_of::<EntityDescriptor>() == 0x10);

/// The u32 entity count lives this many bytes before the entity-list global.
pub const ENTITY_COUNT_BEFORE_LIST: usize = 0x10;

/// Interned, refcounted engine string (node of the global string pool, `lib::HashedString`).
#[repr(C)]
pub struct HashedStr {
    _tree: [usize; 4],   // 0x00 pool tree links
    pub hash: u32,       // 0x20
    _pad_24: u32,        // 0x24
    pub text: *const u8, // 0x28 NUL-terminated (inline at 0x38)
    pub refs: usize,     // 0x30
}
const _: () = assert!(size_of::<HashedStr>() == 0x38 && offset_of!(HashedStr, text) == 0x28);

/// One active scene state ("q120/DONE", "Route/A", ...), kept sorted by name hash.
#[repr(C)]
pub struct SceneState {
    _unknown: [usize; 2],     // 0x00
    pub name: *mut HashedStr, // 0x10
    _unknown2: usize,         // 0x18
}
const _: () = assert!(size_of::<SceneState>() == 0x20);

/// `hap::scene_state::SceneStateSystem`, a static object in the image; only its state array is read.
#[repr(C)]
pub struct SceneStateSystem {
    _unknown: [u8; 0x98],          // 0x00
    pub states: *const SceneState, // 0x98
    pub count: usize,              // 0xA0
}
const _: () = assert!(offset_of!(SceneStateSystem, count) == 0xA0);

/// Scene action (`hap::Action`): cutscenes (PhaseEventAction), triggers, quest steps placed in the layout.
#[repr(C)]
pub struct SceneAction {
    pub vtable: usize, // 0x00
    _unknown: [u8; 0x10], // 0x08
    pub id: u32,       // 0x18 layout id (the save's Scene/Action keys)
    pub flags: u32,    // 0x1C
    pub state: u32,    // 0x20 state name hash
}
const _: () = assert!(offset_of!(SceneAction, state) == 0x20);

/// A group of scene actions.
#[repr(C)]
pub struct ActionGroup {
    _unknown: [u8; 0x30],              // 0x00
    pub actions: *const *mut SceneAction, // 0x30
    pub count: usize,                  // 0x38
}
const _: () = assert!(offset_of!(ActionGroup, count) == 0x38);

/// Static object holding every loaded action group (the one the save's Scene/Action section is written from).
#[repr(C)]
pub struct ActionScene {
    _unknown: [u8; 0x48],                // 0x00
    pub groups: *const *const ActionGroup, // 0x48
    pub count: usize,                    // 0x50
}
const _: () = assert!(offset_of!(ActionScene, count) == 0x50);

pub mod patterns {
    /// `shl rcx,4; add rcx,[rip+disp]` -> global `EntityDescriptor*`. (pattern, disp offset, instruction end)
    pub const ENTITY_LIST: (&str, usize, usize) =
        ("48 c1 e1 04 48 03 ? ? ? ? ? 8b 01 33 c2 a9 00 ff ff ff 75 0f 48 8b 41 08", 7, 11);
    /// String referenced right before `lea rdx,[possessed_handle]`.
    pub const POSSESSED_STRING: &str = "core_AccelTime_Out";
    /// `lea rdx,[rip+disp]` within 10 bytes after the string reference.
    pub const POSSESSED_LEA: &str = "48 8D 15";
    /// Middle of the per-frame game update; the function starts after the preceding `CC CC CC` padding.
    pub const UPDATE_MIDDLE: &str = "? 89 ? e8 00 00 00 ? 89 ? f0 00 00 00 ? ? ? e8 ? ? ? ? ? ? ? ? ? 00 00";
    /// String referenced right before `lea rcx,[spawner]; call spawn` (within 30 bytes).
    pub const SPAWN_STRING: &str = "MapInstance";
    pub const SPAWN_LEA: &str = "48 8D 0D ? ? ? ? E8";
    /// String referenced right before `jmp assign_ai_routine` (within 10 bytes).
    pub const ROUTINE_STRING: &str = "buddy_pascal";
    /// String referenced right before `mov rax,[script_function_list]` (within 10 bytes).
    pub const SCRIPT_STRING: &str = "EntityLayout.enableCreateEntity";
    pub const SCRIPT_MOV: &str = "48 8B 05";
    /// Near the end of the controller's button processing; the function starts after `CC CC CC`.
    pub const BUTTONS_END: &str = "C7 ? 90 07 00 00 14 00 00 00 48";
    /// `mov [rcx+0x740],rax` in that function, after held_flags is rebuilt from input (rcx = controller).
    pub const HELD_STORE: &str = "48 89 81 40 07 00 00";
    /// In pod_shot, non-player branch: `jne aim_copy; target = entity(pl->pod_target); test rax,rax; je exit`.
    /// Hook at +0x1C (`test rax,rax`); `aim_copy` (`lea rcx,[rdi+0x12800]`) is the `jne` rel8 target at +2.
    pub const POD_NO_TARGET: &str = "85 C0 75 ? 48 8D 97 DC 64 01 00 48 8D 4D B0 E8 ? ? ? ? 48 8B C8 E8 ? ? ? ? 48 85 C0 0F 84";
    /// Per-character pod-program update (starts/ends pod skills from held bit 4); the engine's caller (0x528c70 in
    /// exe 2026-10-09) returns early for non-player-controlled characters, so it never runs for the remote.
    pub const POD_PROGRAM: &str = "4C 8B DC 57 48 81 EC A0 01 00 00 48 8B 05 ? ? ? ? 48 33 C4 48 89 84 24 50 01 00 00 83 B9 60 5B 01 00 00";
    /// `SceneStateSystem.set(&sys, &intern(hash(s), s))` on a literal ("Buddy/9S" in exe 2026-10-09), then
    /// `release(&s)`. From the start: hash call, +0x14 `lea rcx,[pool]`, +0x1B intern call, +0x28 `lea rcx,[sys]`,
    /// +0x2F set call, +0x39 release call. The first match whose set call goes to the script's set is used.
    pub const SCENE_SET_SITE: &str = "E8 ? ? ? ? 41 B9 ? 00 00 00 4C 8D 05 ? ? ? ? 8B D0 48 8D 0D ? ? ? ? E8 ? ? ? ? 48 89 45 ? 48 8D 55 ? 48 8D 0D ? ? ? ? E8 ? ? ? ? 90 48 8D 4D ? E8";
    /// Start of `Action::request_state(this, req)`: returns early when `this->state (0x20) == req->hash`.
    pub const REQUEST_STATE: &str = "48 89 5C 24 10 57 48 83 EC 30 48 8B D9 48 8B FA 8B 0A 39 4B 20";
    /// Flag update then `lea rcx,[action_scene]; call save_scene` (+0x14 is the lea).
    pub const ACTION_SCENE: &str = "8B 05 ? ? ? ? 0F BA F0 1C 0F BA E8 1E 89 05 ? ? ? ? 48 8D 0D ? ? ? ? E8";
    /// Start of the scene signal queue push, `post(queue, &sender, &signal)` (called at +0x53 in Action.signal).
    pub const SIGNAL_POST: &str = "48 8B C4 56 57 41 56 48 83 EC 50 48 C7 40 B8 FE FF FF FF 48 89 58 10";
    /// RTTI type name of the cutscene action class.
    pub const PHASE_EVENT_RTTI: &str = ".?AVPhaseEventAction@hap@@";
    /// RTTI type name of the action that sends commands to entities (blow up debris, open doors).
    pub const SEND_COMMAND_RTTI: &str = ".?AVSendCommand@hap@@";
    /// RTTI type name of a quest objective (goal marker); its state changes are mirrored.
    pub const GOAL_RTTI: &str = ".?AVHapGoalLocationAction@hap@@";
    /// RTTI type name of an area-triggered enemy spawn set; the host's spawn start is mirrored.
    pub const ENEMY_SET_RTTI: &str = ".?AVEnemySetArea@hap@@";
    /// `SendCommand` vtable slot that runs its command list, then requests DONE: `run(this, out)`.
    pub const SEND_COMMAND_RUN: usize = 9;
    /// `HapGoalLocationAction` vtable slot `update(this)`: in WAIT it sets the HUD objective and map marker, then
    /// requests ACTIVE (exe 2026-10-09: 0x3e34c0).
    pub const GOAL_UPDATE: usize = 9;
    /// `HapGoalLocationAction` vtable slot `finish(this)`: removes its map marker and HUD objective, then requests
    /// DONE (exe 2026-10-09: 0x3e07f0).
    pub const GOAL_FINISH: usize = 12;
    /// First bytes at `aim_copy`: `lea rcx,[rdi+0x12800]`.
    pub const AIM_COPY: [u8; 7] = [0x48, 0x8D, 0x8F, 0x00, 0x28, 0x01, 0x00];
}
