use varlit::{VarlitFile, VarlitReader};

fn map_payloads(reader: &VarlitReader) {
    let _mapping = reader.mmap_payloads();
}

fn map_reader_matrix(reader: &VarlitReader) {
    let _mapping = reader.mmap_matrix();
}

fn map_file_payloads(file: &VarlitFile) {
    let _mapping = file.mmap_payloads();
}

fn map_file_matrix(file: &VarlitFile) {
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
