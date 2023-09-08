// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests images support in the API

use dropshot::ResultsPage;
use http::method::Method;
use http::StatusCode;
use nexus_test_utils::http_testing::AuthnMode;
use nexus_test_utils::http_testing::NexusRequest;
use nexus_test_utils::http_testing::RequestBuilder;
use nexus_test_utils::resource_helpers::create_project;
use nexus_test_utils::resource_helpers::DiskTest;
use nexus_test_utils_macros::nexus_test;
use omicron_common::api::external::Disk;

use nexus_types::external_api::{params, views};
use omicron_common::api::external::{ByteCount, IdentityMetadataCreateParams};

use httptest::{matchers::*, responders::*, Expectation, ServerBuilder};

type ControlPlaneTestContext =
    nexus_test_utils::ControlPlaneTestContext<omicron_nexus::Server>;

const PROJECT_NAME: &str = "myproj";

const BLOCK_SIZE: params::BlockSize = params::BlockSize(512);

fn get_project_images_url(project_name: &str) -> String {
    format!("/v1/images?project={}", project_name)
}

fn get_image_create(source: params::ImageSource) -> params::ImageCreate {
    params::ImageCreate {
        identity: IdentityMetadataCreateParams {
            name: "alpine-edge".parse().unwrap(),
            description: String::from(
                "you can boot any image, as long as it's alpine",
            ),
        },
        os: "alpine".to_string(),
        version: "edge".to_string(),
        source,
    }
}

#[nexus_test]
async fn test_image_create(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header(
                    "Content-Length",
                    format!("{}", 4096 * 1000),
                ),
            ),
    );

    let images_url = get_project_images_url(PROJECT_NAME);

    // Before project exists, image list 404s
    NexusRequest::expect_failure(
        client,
        StatusCode::NOT_FOUND,
        Method::GET,
        &images_url,
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("Expected 404");

    // create the project, now we expect an empty list
    let _project = create_project(client, PROJECT_NAME).await;

    let images = NexusRequest::object_get(client, &images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(images.len(), 0);

    // Create an image in the project
    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    NexusRequest::objects_post(client, &images_url, &image_create_params)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<views::Image>()
        .await;

    // one image in the project
    let images = NexusRequest::object_get(client, &images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(images.len(), 1);
    assert_eq!(images[0].identity.name, "alpine-edge");
}

#[nexus_test]
async fn test_silo_image_create(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header(
                    "Content-Length",
                    format!("{}", 4096 * 1000),
                ),
            ),
    );

    let silo_images_url = "/v1/images";

    // Expect no images in the silo
    let images = NexusRequest::object_get(client, &silo_images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(images.len(), 0);

    // Create an image in the project
    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    // Create image
    NexusRequest::objects_post(client, &silo_images_url, &image_create_params)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<views::Image>()
        .await;

    let images = NexusRequest::object_get(client, &silo_images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(images.len(), 1);
    assert_eq!(images[0].identity.name, "alpine-edge");
}

#[nexus_test]
async fn test_image_create_url_404(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    // need a project to post to
    create_project(client, PROJECT_NAME).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(status_code(404)),
    );

    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    let images_url = get_project_images_url(PROJECT_NAME);

    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &images_url)
            .body(Some(&image_create_params))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(
        error.message,
        format!("unsupported value for \"url\": querying url returned: 404 Not Found")
    );
}

#[nexus_test]
async fn test_image_create_bad_url(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    // need a project to post to
    create_project(client, PROJECT_NAME).await;

    let image_create_params = get_image_create(params::ImageSource::Url {
        url: "not_a_url".to_string(),
        block_size: BLOCK_SIZE,
    });

    let images_url = get_project_images_url(PROJECT_NAME);

    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &images_url)
            .body(Some(&image_create_params))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(
        error.message,
        format!("unsupported value for \"url\": error querying url: builder error: relative URL without a base")
    );
}

#[nexus_test]
async fn test_image_create_bad_content_length(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    // need a project to post to
    create_project(client, PROJECT_NAME).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header("Content-Length", "bad"),
            ),
    );

    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    let images_url = get_project_images_url(PROJECT_NAME);

    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &images_url)
            .body(Some(&image_create_params))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(
        error.message,
        format!("unsupported value for \"url\": content length invalid: invalid digit found in string")
    );
}

#[nexus_test]
async fn test_image_create_bad_image_size(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    // need a project to post to
    create_project(client, PROJECT_NAME).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(status_code(200).append_header(
                "Content-Length",
                format!("{}", 4096 * 1000 + 100),
            )),
    );

    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    let images_url = get_project_images_url(PROJECT_NAME);

    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &images_url)
            .body(Some(&image_create_params))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(
        error.message,
        format!("unsupported value for \"size\": total size {} must be divisible by block size {}", 4096*1000 + 100, 512)
    );
}

#[nexus_test]
async fn test_make_disk_from_image(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    // need a project to post both disk and image to
    create_project(client, PROJECT_NAME).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/alpine/edge.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header(
                    "Content-Length",
                    format!("{}", 4096 * 1000),
                ),
            ),
    );

    // Create an image in the project
    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/alpine/edge.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    let images_url = get_project_images_url(PROJECT_NAME);

    let alpine_image =
        NexusRequest::objects_post(client, &images_url, &image_create_params)
            .authn_as(AuthnMode::PrivilegedUser)
            .execute_and_parse_unwrap::<views::Image>()
            .await;

    let new_disk = params::DiskCreate {
        identity: IdentityMetadataCreateParams {
            name: "disk".parse().unwrap(),
            description: String::from("sells rainsticks"),
        },
        disk_source: params::DiskSource::Image {
            image_id: alpine_image.identity.id,
        },
        size: ByteCount::from_gibibytes_u32(1),
    };

    let disks_url = format!("/v1/disks?project={}", PROJECT_NAME);
    NexusRequest::objects_post(client, &disks_url, &new_disk)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute()
        .await
        .unwrap();
}

#[nexus_test]
async fn test_make_disk_from_other_project_image_fails(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    create_project(client, PROJECT_NAME).await;
    let another_project = create_project(client, "another-proj").await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header(
                    "Content-Length",
                    format!("{}", 4096 * 1000),
                ),
            ),
    );

    let images_url = get_project_images_url(PROJECT_NAME);
    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });
    let image =
        NexusRequest::objects_post(client, &images_url, &image_create_params)
            .authn_as(AuthnMode::PrivilegedUser)
            .execute_and_parse_unwrap::<views::Image>()
            .await;

    let new_disk = params::DiskCreate {
        identity: IdentityMetadataCreateParams {
            name: "stolen-disk".parse().unwrap(),
            description: String::from("yoink"),
        },
        disk_source: params::DiskSource::Image { image_id: image.identity.id },
        size: ByteCount::from_gibibytes_u32(1),
    };
    let disks_url =
        format!("/v1/disks?project={}", another_project.identity.name);
    let error = NexusRequest::expect_failure_with_body(
        client,
        StatusCode::BAD_REQUEST,
        Method::POST,
        &disks_url,
        &new_disk,
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("expected 400")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(error.message, "image does not belong to this project");
}

#[nexus_test]
async fn test_make_disk_from_image_too_small(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    // need a project to post both disk and image to
    create_project(client, PROJECT_NAME).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/alpine/edge.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header("Content-Length", "2147483648"),
            ),
    );

    // Create an image in the project
    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/alpine/edge.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    let images_url = get_project_images_url(PROJECT_NAME);

    let alpine_image =
        NexusRequest::objects_post(client, &images_url, &image_create_params)
            .authn_as(AuthnMode::PrivilegedUser)
            .execute_and_parse_unwrap::<views::Image>()
            .await;

    let new_disk = params::DiskCreate {
        identity: IdentityMetadataCreateParams {
            name: "disk".parse().unwrap(),
            description: String::from("sells rainsticks"),
        },
        disk_source: params::DiskSource::Image {
            image_id: alpine_image.identity.id,
        },
        size: ByteCount::from(1073741824),
    };

    let disks_url = format!("/v1/disks?project={}", PROJECT_NAME);
    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &disks_url)
            .body(Some(&new_disk))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(
        error.message,
        format!(
            "disk size {} must be greater than or equal to image size {}",
            1073741824_u32, 2147483648_u32,
        )
    );
}

#[nexus_test]
async fn test_image_promotion(cptestctx: &ControlPlaneTestContext) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header(
                    "Content-Length",
                    format!("{}", 4096 * 1000),
                ),
            ),
    );

    let silo_images_url = "/v1/images";
    let images_url = get_project_images_url(PROJECT_NAME);

    create_project(client, PROJECT_NAME).await;

    let images = NexusRequest::object_get(client, &images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(images.len(), 0);

    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });

    NexusRequest::objects_post(client, &images_url, &image_create_params)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<views::Image>()
        .await;

    let project_images = NexusRequest::object_get(client, &images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(project_images.len(), 1);
    assert_eq!(project_images[0].identity.name, "alpine-edge");

    let image_id = project_images[0].identity.id;
    let project_image_url = format!("/v1/images/{}", image_id);

    let project_image = NexusRequest::object_get(client, &project_image_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<views::Image>()
        .await;

    assert_eq!(project_image.identity.id, image_id);

    // promote the image to the silo
    let promote_url = format!("/v1/images/{}/promote", image_id);
    NexusRequest::new(
        RequestBuilder::new(client, http::Method::POST, &promote_url)
            .expect_status(Some(http::StatusCode::ACCEPTED)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute_and_parse_unwrap::<views::Image>()
    .await;

    let silo_images = NexusRequest::object_get(client, &silo_images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(silo_images.len(), 1);
    assert_eq!(silo_images[0].identity.name, "alpine-edge");

    // Ensure there are no more project images
    let project_images = NexusRequest::object_get(client, &images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(project_images.len(), 0);

    let silo_image_url = format!("/v1/images/{}", image_id);
    let silo_image = NexusRequest::object_get(client, &silo_image_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<views::Image>()
        .await;

    assert_eq!(silo_image.identity.id, image_id);

    // Create another project image with the same name
    NexusRequest::objects_post(client, &images_url, &image_create_params)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<views::Image>()
        .await;

    // Ensure project image was created
    let project_images = NexusRequest::object_get(client, &images_url)
        .authn_as(AuthnMode::PrivilegedUser)
        .execute_and_parse_unwrap::<ResultsPage<views::Image>>()
        .await
        .items;

    assert_eq!(project_images.len(), 1);
    assert_eq!(project_images[0].identity.name, "alpine-edge");

    let image_id = project_images[0].identity.id;
    let promote_url = format!("/v1/images/{}/promote", image_id);

    NexusRequest::new(
        RequestBuilder::new(client, http::Method::POST, &promote_url)
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
}

#[nexus_test]
async fn test_image_from_other_project_snapshot_fails(
    cptestctx: &ControlPlaneTestContext,
) {
    let client = &cptestctx.external_client;
    DiskTest::new(&cptestctx).await;

    let server = ServerBuilder::new().run().unwrap();
    server.expect(
        Expectation::matching(request::method_path("HEAD", "/image.raw"))
            .times(1..)
            .respond_with(
                status_code(200).append_header(
                    "Content-Length",
                    format!("{}", 4096 * 1000),
                ),
            ),
    );

    create_project(client, PROJECT_NAME).await;
    let images_url = get_project_images_url(PROJECT_NAME);
    let disks_url = format!("/v1/disks?project={}", PROJECT_NAME);
    let snapshots_url = format!("/v1/snapshots?project={}", PROJECT_NAME);

    // Create an image
    let image_create_params = get_image_create(params::ImageSource::Url {
        url: server.url("/image.raw").to_string(),
        block_size: BLOCK_SIZE,
    });
    let image: views::Image =
        NexusRequest::objects_post(client, &images_url, &image_create_params)
            .authn_as(AuthnMode::PrivilegedUser)
            .execute_and_parse_unwrap()
            .await;
    // Create a disk from this image
    let disk_create_params = params::DiskCreate {
        identity: IdentityMetadataCreateParams {
            name: "disk".parse().unwrap(),
            description: "meow".into(),
        },
        disk_source: params::DiskSource::Image { image_id: image.identity.id },
        size: ByteCount::from_gibibytes_u32(1),
    };
    let disk: Disk =
        NexusRequest::objects_post(client, &disks_url, &disk_create_params)
            .authn_as(AuthnMode::PrivilegedUser)
            .execute()
            .await
            .unwrap()
            .parsed_body()
            .unwrap();
    // Create a snapshot from this disk
    let snapshot_create_params = params::SnapshotCreate {
        identity: IdentityMetadataCreateParams {
            name: "snapshot".parse().unwrap(),
            description: "meow".into(),
        },
        disk: disk.identity.id.into(),
    };
    let snapshot: views::Snapshot = NexusRequest::objects_post(
        client,
        &snapshots_url,
        &snapshot_create_params,
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .unwrap()
    .parsed_body()
    .unwrap();

    // Create a second project and ensure image creation fails
    let another_project = create_project(client, "another-proj").await;
    let images_url =
        get_project_images_url(another_project.identity.name.as_str());
    let image_create_params = get_image_create(params::ImageSource::Snapshot {
        id: snapshot.identity.id,
    });
    let error = NexusRequest::new(
        RequestBuilder::new(client, Method::POST, &images_url)
            .body(Some(&image_create_params))
            .expect_status(Some(StatusCode::BAD_REQUEST)),
    )
    .authn_as(AuthnMode::PrivilegedUser)
    .execute()
    .await
    .expect("unexpected success")
    .parsed_body::<dropshot::HttpErrorResponseBody>()
    .unwrap();
    assert_eq!(error.message, "snapshot does not belong to this project");
}
