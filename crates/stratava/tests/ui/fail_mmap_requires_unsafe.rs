use stratava::{StratavaFile, StratavaReader};

fn map_payloads(reader: &StratavaReader) {
    let _mapping = reader.mmap_payloads();
}

fn map_reader_matrix(reader: &StratavaReader) {
    let _mapping = reader.mmap_matrix();
}

fn map_file_payloads(file: &StratavaFile) {
    let _mapping = file.mmap_payloads();
}

fn map_file_matrix(file: &StratavaFile) {
    let _mapping = file.mmap_matrix();
}

fn main() {
    let _ = (
        map_payloads,
        map_reader_matrix,
        map_file_payloads,
        map_file_matrix,
    );
}
