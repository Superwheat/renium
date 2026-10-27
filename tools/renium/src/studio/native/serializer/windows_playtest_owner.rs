//! Bind the Edit launch and its child-model task queue before Play starts.
use super::*;

pub(super) const EXECUTE: &str = ".?AV?$BoundYieldFuncDesc@VStudioTestService@Studio@RBX@@$$A6A?AVVariant@Reflection@3@V453@@Z$0A@$00@Reflection@RBX@@";
pub(super) const ARGS: &str = ".?AV?$BoundFuncDesc@VStudioTestService@Studio@RBX@@$$A6A?AVVariant@Reflection@3@XZ$0A@$0A@@Reflection@RBX@@";

pub(in super::super) struct Binding {
    pub execute: (usize, usize),
    pub fields: [usize; 16],
    pub place: i64,
    pub universe: i64,
}

pub(in super::super) fn resolve(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    model: &ActiveDataModel,
) -> Result<Binding> {
    anyhow::ensure!(types::yield_abi(memory, studio, EXECUTE, "ExecutePlayModeAsync", 1)?
        == types::yield_abi(memory, studio, dispatch::INVOKE, "Invoke", 1)?,
        "Native launch continuation ABIs disagree");
    let execute = types::yield_member(memory, studio, EXECUTE, "ExecutePlayModeAsync", 1)?;
    let (fields, place, universe) = task_binding(memory, studio, model)?;
    Ok(Binding {
        execute,
        fields,
        place,
        universe,
    })
}

/// Resolve the model's queue independently of the launch dispatch. During Play
/// that dispatch is already owned by the teleport lease and is not callable
/// through a fresh reflection binding.
pub(super) fn task_binding(
    memory: &ProcessMemory,
    studio: &ModuleEntry,
    model: &ActiveDataModel,
) -> Result<([usize; 16], i64, i64)> {
    let layout = package_layout(&studio.path)?;
    let source = model.outer + model.layout.data_model_instance;
    let test = model
        .roots
        .iter()
        .filter(|entry| {
            read_instance_class(memory, entry.instance, model.layout).as_deref()
                == Some("StudioTestService")
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        test.len() == 1,
        "The Edit model has no unique test controller"
    );
    let (_, args) = types::member(memory, studio, ARGS, "GetTestArgs", false)?;
    let place = integer_property_layout(memory, studio, source, model.layout, "PlaceId")?;
    let universe = integer_property_layout(memory, studio, source, model.layout, "GameId")?;
    anyhow::ensure!(
        place.width == 8 && universe.width == 8,
        "Native launch identity widths changed"
    );
    let identity = (
        read_integer_property(memory, source, place)?,
        read_integer_property(memory, source, universe)?,
    );
    anyhow::ensure!(
        identity.0 > 0 && identity.1 > 0,
        "Play teleport needs a published source place"
    );
    let launch = loader::nonce_bindings(memory, studio, model)?;
    let context = data_model_task_binding(memory, studio, &layout, model)?.field;
    Ok((
        [
            source,
            test[0].instance,
            args,
            properties::parent_offset(memory, model)?,
            model.layout.children,
            model.layout.class_descriptor,
            place.offset,
            universe.offset,
            types::singleton(memory, studio, dispatch::TABLE, None)?,
            launch[0],
            launch[1],
            model.layout.self_pointer,
            context.offset,
            context.mask,
            memory.read_u64(context.pointer)? as usize,
            studio.base + layout.submit_task,
        ],
        identity.0,
        identity.1,
    ))
}
