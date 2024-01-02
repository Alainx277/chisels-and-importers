use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use base64::Engine;
use bitstream_io::{BitWrite, BitWriter};
use clap::Parser;
use fastnbt::ByteArray;
use lz4_flex::frame::FrameEncoder;
use palette::{color_difference::Ciede2000, IntoColor, Lch, Srgb};
use serde::Serialize;

/// Convert Magica Voxel models into Chisels and Bits patterns
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help(true))]
#[command(help_template(
    "\
{before-help}{name} {version} by {author}
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}
"
))]
struct Args {
    /// path to Magica Voxel file (typically .vox)
    #[arg()]
    model: String,
    /// the filename for the resulting pattern(s)
    #[arg(short, long, default_value = "pattern")]
    output: String,
    /// what block palette file to use
    #[arg(short, long, default_value = "blocks.json")]
    palette: String,
    #[clap(flatten)]
    model_group: ModelGroup,
}

#[derive(Debug, clap::Args)]
#[group(required = false, multiple = false)]
pub struct ModelGroup {
    /// create pattern(s) for each model in the file
    #[clap(short, long)]
    all_models: bool,
    /// create pattern(s) for specific models in the file
    #[clap(short, long, value_delimiter = ',', num_args = 1..)]
    models: Option<Vec<usize>>,
}

fn main() {
    let args = Args::parse();

    let voxel_file = &args.model;
    let voxel_data = dot_vox::load(&voxel_file).expect("parsing voxel file");

    let mapping_raw = std::fs::read(&args.palette).expect("missing palette");
    let block_palette = BlockPalette::from_json(&mapping_raw);

    let mut models = Vec::new();
    let model_count = voxel_data.models.len();
    if model_count == 1 || args.model_group.all_models {
        models.extend(voxel_data.models.iter());
    } else if let Some(requested) = args.model_group.models {
        for index in requested.iter() {
            models.push(
                voxel_data
                    .models
                    .get(index - 1)
                    .expect("invalid model index"),
            );
        }
    } else {
        eprintln!("Multiple models inside file ({}), pass -a to export all models or -m to export specific models", model_count);
        return;
    }

    let export_count = models.len();
    for (i, model) in models.into_iter().enumerate() {
        let prefix = if export_count == 1 {
            args.output.clone()
        } else {
            format!("{}_{}", &args.output, i)
        };

        create_patterns(model, &block_palette, &voxel_data, &prefix);
    }
}

const PATTERN_EXTENSION: &'static str = ".cbsbp";

fn create_patterns(
    model: &dot_vox::Model,
    block_palette: &BlockPalette,
    voxel_data: &dot_vox::DotVoxData,
    path_prefix: &str,
) {
    // Build an O(1) lookup array for voxels
    let mut model_data: Vec<Option<u8>> =
        vec![None; VOXEL_MAX_SIDE * VOXEL_MAX_SIDE * VOXEL_MAX_SIDE];
    let mut used_colors = HashSet::<_>::default();
    for voxel in model.voxels.iter() {
        let index = index_from_position(voxel.x, voxel.y, voxel.z);
        model_data[index] = Some(voxel.i);
        used_colors.insert(voxel.i);
    }
    let model_data = model_data.into_boxed_slice();

    // Translate voxel palette into block palette
    let mut palette_mapping = HashMap::new();
    let mut chisel_palette = Vec::with_capacity(used_colors.len() + 1);
    for vox_palette_index in used_colors {
        let vox_color = voxel_data.palette.get(vox_palette_index as usize).unwrap();
        let closest_block = block_palette.closest_block(*vox_color);

        palette_mapping.insert(vox_palette_index, chisel_palette.len() as u8);
        chisel_palette.push(PaletteEntry {
            state: format!("{{\"Name\":\"{}\"}}", closest_block),
        });
    }
    // Last entry is always air
    chisel_palette.push(PaletteEntry {
        state: "{\"Name\":\"minecraft:air\"}".to_owned(),
    });

    // Divide voxel model into block sized chunks and create a pattern for each
    let size = model.size;
    let length = (size.x as f32 / BLOCK_SIDE as f32).ceil() as usize;
    let width = (size.y as f32 / BLOCK_SIDE as f32).ceil() as usize;
    let height = (size.z as f32 / BLOCK_SIDE as f32).ceil() as usize;
    let one_pattern = length == 1 && width == 1 && height == 1;

    let mut index = 0;
    for x in 0..length {
        for y in 0..width {
            for z in 0..height {
                let offset = (
                    (x * BLOCK_SIDE) as u8,
                    (y * BLOCK_SIDE) as u8,
                    (z * BLOCK_SIDE) as u8,
                );
                let Some((data, statistics)) =
                    model_to_data(&model_data, &chisel_palette, &palette_mapping, offset)
                else {
                    continue;
                };

                let pattern = data_to_pattern(
                    ChiselData {
                        data: ByteArray::new(data),
                        palette: &chisel_palette,
                    },
                    statistics,
                );

                let output_file = if one_pattern {
                    format!("{}{}", path_prefix, PATTERN_EXTENSION)
                } else {
                    format!("{}_{}{}", path_prefix, index, PATTERN_EXTENSION)
                };
                std::fs::write(output_file, &pattern).expect("failed to write pattern file");
                index += 1;
            }
        }
    }
}

struct BlockPalette {
    mapping: Vec<(Lch, String)>,
}

impl BlockPalette {
    fn from_json(data: &[u8]) -> Self {
        let block_mapping: HashMap<String, String> =
            serde_json::from_slice(data).expect("invalid json in palette");
        let mapping = block_mapping
            .into_iter()
            .map(|(k, v)| {
                (
                    Srgb::from_str(&k)
                        .expect("invalid color code in palette")
                        .into_linear::<f32>()
                        .into_color(),
                    v,
                )
            })
            .collect();

        Self { mapping }
    }

    fn closest_block(&self, color: dot_vox::Color) -> &str {
        let color = Srgb::new(color.r, color.g, color.b);
        let color: Lch = color.into_linear::<f32>().into_color();

        // Select best matching block
        let mut color_diffs: Vec<_> = self
            .mapping
            .iter()
            .map(|(block_color, block)| (block_color.difference(color), block))
            .collect();
        color_diffs.sort_by(|(l, _), (r, _)| l.total_cmp(r));
        let block_name = color_diffs.first().unwrap().1;
        block_name.as_str()
    }
}

type ModelData = Box<[Option<u8>]>;

fn data_to_pattern(data: ChiselData, statistics: Statistics) -> Vec<u8> {
    let output_data = Data {
        chiseled_data: data,
        statistics,
    };

    let chisel_nbt = fastnbt::to_bytes(&output_data).unwrap();
    // Compress chisel nbt with lz4
    let mut compressed_chisel_nbt = Vec::new();
    let mut lz4_encoder = FrameEncoder::new(&mut compressed_chisel_nbt);
    std::io::copy(&mut chisel_nbt.as_slice(), &mut lz4_encoder).unwrap();
    lz4_encoder.finish().unwrap();

    let container = DataContainer {
        version: 0,
        data: CompressedData {
            data: ByteArray::new(compressed_chisel_nbt.into_iter().map(|b| b as i8).collect()),
            compressed: 1u8,
        },
    };
    let container_nbt = fastnbt::to_bytes(&container).unwrap();
    let nbt_base64 = base64::engine::general_purpose::STANDARD.encode(&container_nbt);

    // Create pattern JSON
    let pattern = PatternFile {
        version: "1.0",
        chisel_data: nbt_base64,
    };
    let pattern_bytes = serde_json::to_vec(&pattern).unwrap();
    let pattern_string = base64::engine::general_purpose::STANDARD.encode(&pattern_bytes);
    // zlib compress pattern
    let compressed_pattern =
        miniz_oxide::deflate::compress_to_vec_zlib(pattern_string.as_bytes(), 6);
    compressed_pattern
}

fn model_to_data<'a>(
    model: &ModelData,
    palette: &'a [PaletteEntry],
    palette_mapping: &HashMap<u8, u8>,
    offset: (u8, u8, u8),
) -> Option<(Vec<i8>, Statistics<'a>)> {
    let total_size = BLOCK_SIDE * BLOCK_SIDE * BLOCK_SIDE;

    let mut data: Vec<u8> = Vec::with_capacity(total_size);
    let mut writer = BitWriter::endian(&mut data, bitstream_io::LittleEndian);

    let mut block_states = Vec::with_capacity(palette.len());
    for entry in palette {
        block_states.push(BlockState {
            block_information: entry,
            count: 0,
        });
    }

    let entry_width = f32::log2(palette.len() as f32).ceil() as u32;
    let mut only_air = true;
    for i in 0..total_size {
        let (mut x, mut y, mut z) = position_from_index(i);
        x += offset.1;
        y += offset.2;
        z += offset.0;

        let index = index_from_position(z, x, y);
        let voxel = model[index];
        let val = if let Some(v) = voxel {
            // If voxel is present get mapped index
            only_air = false;
            *palette_mapping.get(&v).unwrap()
        } else {
            // Last palette entry is air
            (palette.len() - 1) as u8
        };
        writer.write(entry_width, val).unwrap();
        block_states.get_mut(val as usize).unwrap().count += 1;
    }

    if only_air {
        return None;
    }

    Some((
        data.into_iter().map(|b| b as i8).collect(),
        Statistics {
            primary_state: palette.get(0).unwrap(),
            block_states,
        },
    ))
}

const BLOCK_SIDE: usize = 16;

fn position_from_index(index: usize) -> (u8, u8, u8) {
    let x = index / (BLOCK_SIDE * BLOCK_SIDE);
    let y = (index - x * BLOCK_SIDE * BLOCK_SIDE) / BLOCK_SIDE;
    let z = index - x * BLOCK_SIDE * BLOCK_SIDE - y * BLOCK_SIDE;
    (x as u8, y as u8, z as u8)
}

const VOXEL_MAX_SIDE: usize = 255;

fn index_from_position(x: u8, y: u8, z: u8) -> usize {
    x as usize * VOXEL_MAX_SIDE * VOXEL_MAX_SIDE + y as usize * VOXEL_MAX_SIDE + z as usize
}

#[derive(Serialize)]
struct Data<'a> {
    #[serde(rename = "chiseledData")]
    chiseled_data: ChiselData<'a>,
    statistics: Statistics<'a>,
}

#[derive(Serialize)]
struct ChiselData<'a> {
    data: ByteArray,
    palette: &'a [PaletteEntry],
}

#[derive(Serialize)]
struct Statistics<'a> {
    #[serde(rename = "primaryState")]
    primary_state: &'a PaletteEntry,
    #[serde(rename = "blockStates")]
    block_states: Vec<BlockState<'a>>,
}

#[derive(Serialize)]
struct BlockState<'a> {
    block_information: &'a PaletteEntry,
    count: u32,
}

#[derive(Serialize, Clone)]
struct PaletteEntry {
    state: String,
}

#[derive(Serialize)]
struct DataContainer {
    version: u32,
    data: CompressedData,
}

#[derive(Serialize)]
struct CompressedData {
    data: ByteArray,
    compressed: u8,
}

#[derive(Serialize)]
struct PatternFile {
    #[serde(rename = "chiselData")]
    chisel_data: String,
    version: &'static str,
}
