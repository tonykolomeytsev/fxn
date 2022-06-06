use std::collections::HashMap;

use crate::api::figma::FigmaApi;
use crate::common::error::AppError;
use crate::common::fetcher::{fetch, FetcherTarget};
use crate::common::fileutils::{create_dir, move_file};
use crate::common::http_client::create_http_client;
use crate::common::renderer::Renderer;
use crate::common::res_name::to_res_name;
use crate::common::suggestions::generate_name_suggections;
use crate::common::webp;
use crate::feature_images::view::View;
use crate::models::config::{AppConfig, ImageFormat};

#[derive(Debug, Clone)]
struct ImageInfo {
    name: String,
    scale_name: String,
    scale_value: f32,
    format: ImageFormat,
    res_name: String,
}

pub fn export_images(token: &String, image_names: &Vec<String>, yaml_config_path: &String) {
    let renderer = Renderer();
    let api = FigmaApi::new(create_http_client(&token));

    let fetcher_entry = match fetch(&api, &yaml_config_path, FetcherTarget::Images, &renderer) {
        Ok(fetcher_entry) => fetcher_entry,
        Err(e) => {
            renderer.render(View::Error(format!("{}", e)));
            return;
        }
    };
    let (app_config, names_to_ids) = (fetcher_entry.app_config, fetcher_entry.image_names_to_ids);

    // If `android.images.format` is SVG, export only one scale (x1)
    let single_scale;
    let image_scales = if app_config.android.images.format.is_svg() {
        single_scale = HashMap::from([(String::new(), 1f32)]);
        &single_scale
    } else {
        &app_config.android.images.scales
    };

    for image_name in image_names {
        for (scale_name, scale_value) in image_scales {
            // Just to not to pass long parameter list to export_image function
            let image_info = ImageInfo {
                name: image_name.clone(),
                scale_name: scale_name.clone(),
                scale_value: *scale_value,
                format: app_config.android.images.format.clone(),
                res_name: to_res_name(&image_name),
            };
            let export_result =
                export_image(&api, &app_config, &image_info, &names_to_ids, &renderer);

            match &export_result {
                Err(AppError::ImageMissingInFrame(_, _, _)) => (), // We will handle in next statement
                Err(e) => renderer.render(View::Error(e.to_string())),
                Ok(()) => (),
            }

            // Render export result in terminal and stop export of the image, if it is missing in frame.
            if check_image_missing_error(export_result, &renderer) {
                break;
            }

            renderer.new_line();
        }
    }

    renderer.render(View::Done { message: None });
}

fn export_image(
    api: &FigmaApi,
    app_config: &AppConfig,
    image_info: &ImageInfo,
    names_to_ids: &HashMap<String, String>,
    renderer: &Renderer,
) -> Result<(), AppError> {
    let file_id = &app_config.figma.file_id;
    let quality = app_config.android.images.webp_options.quality;

    // Find image frame id by its name
    let node_id = names_to_ids.get(&image_info.name).ok_or_else(|| {
        // If we can't find desired image by name, offer a suggestions
        let frame_name = &app_config.common.images.figma_frame_name;
        let available_names = names_to_ids
            .iter()
            .map(|(k, _)| k.clone())
            .collect::<Vec<String>>();
        let suggestions = generate_name_suggections(&image_info.name, &available_names);
        AppError::ImageMissingInFrame(image_info.name.clone(), frame_name.clone(), suggestions)
    })?;

    // Get download url for exported image
    renderer.render(View::FetchingImage(
        image_info.name.clone(),
        image_info.scale_name.clone(),
    ));
    let image_download_url =
        api.get_image_download_url(file_id, node_id, image_info.scale_value, &image_info.format)?;

    // Download image from gotten url to app's TEMPORARY dir
    renderer.render(View::DownloadingImage(
        image_info.name.clone(),
        image_info.scale_name.clone(),
    ));
    let image_format = &app_config.android.images.format;
    let image_temporary_file_name = api.get_image(
        &image_download_url,
        &image_info.res_name,
        &image_info.scale_name,
        &image_format,
    )?;

    // So... Convert if necessary :)
    let image_temporary_file_name =
        convert_to_webp_if_necessary(&image_info, image_temporary_file_name, quality, &renderer)?;

    // Create drawable-XXXX dir in res dir of android project
    let res_dir = &app_config
        .main_res_images()
        .expect("Validation is done in fetcher");
    let full_final_image_dir = image_drawable_dir(&res_dir, &image_info);
    create_dir(&full_final_image_dir)
        .map_err(|e| AppError::CannotCreateDrawableDir(e.to_string()))?;

    // Move image from temporary dir to drawable dir of android project
    let extension = image_info.format.extension();
    let full_final_image_path = format!(
        "{}/{}.{}",
        full_final_image_dir, &image_info.res_name, &extension
    );
    move_file(&image_temporary_file_name, &full_final_image_path)
        .map_err(|e| AppError::CannotMoveToDrawableDir(image_info.name.clone(), e.to_string()))?;

    // Tell the user that we are done exporting image for this scale
    renderer.render(View::ImageExported(
        image_info.name.clone(),
        image_info.scale_name.clone(),
    ));
    Ok(())
}

fn convert_to_webp_if_necessary(
    image_info: &ImageInfo,
    image_file_name: String,
    quality: f32,
    renderer: &Renderer,
) -> Result<String, AppError> {
    match image_info.format {
        ImageFormat::Webp => {
            renderer.render(View::ConvertingToWebp(
                image_info.name.clone(),
                image_info.scale_name.clone(),
            ));
            let new_image_path = webp::image_to_webp(&image_file_name, quality)?;
            renderer.render(View::ConvertedToWebp(
                image_info.name.clone(),
                image_info.scale_name.clone(),
            ));
            Ok(new_image_path)
        }
        _ => Ok(image_file_name),
    }
}

/// If we've encounter [AppError::ImageMissingInFrame] error and have suggestions gotten with the error,
/// render the error description and the suggestions.
///
/// Returns `true` if we must stop further export process for all other scales of the image.
///
/// Returns `false` otherwise.
fn check_image_missing_error(export_result: Result<(), AppError>, renderer: &Renderer) -> bool {
    match export_result {
        Err(AppError::ImageMissingInFrame(name, frame, Some(suggestions))) => {
            renderer.render(View::ErrorWithSuggestions(
                format!("An image `{}` is missing in frame `{}`, but there are images with similar names:", name, frame),
                suggestions,
            ));
            renderer.new_line();
            true // stop export process
        }
        Err(AppError::ImageMissingInFrame(name, frame, None)) => {
            renderer.render(View::Error(format!(
                "An image `{}` is missing in frame `{}`",
                name, frame,
            )));
            renderer.new_line();
            true // stop export process
        }
        _ => false, // continue export process
    }
}

/// Always returns `.../res/drawable` for SVG images.
///
/// Returns `.../res/drawablee-{scale_name}` for images with other formats.
fn image_drawable_dir(res_dir: &String, image_info: &ImageInfo) -> String {
    if image_info.format.is_svg() {
        format!("{}/drawable", &res_dir)
    } else {
        format!("{}/drawable-{}", &res_dir, &image_info.scale_name)
    }
}
