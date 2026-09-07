terraform {
  required_version = ">= 1.6.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "bucket_prefix" {
  type    = string
  default = "workcell"
}

provider "aws" {
  region = var.region
}

resource "aws_s3_bucket" "logs" {
  bucket        = "${var.bucket_prefix}-logs"
  force_destroy = false

  tags = merge(local.common_tags, { Purpose = "logs" })
}

resource "aws_s3_bucket_versioning" "logs" {
  bucket = aws_s3_bucket.logs.id

  versioning_configuration {
    status = "Enabled"
  }
}

locals {
  common_tags = {
    Owner       = "platform"
    Environment = "production"
  }
}

output "bucket_name" {
  value       = aws_s3_bucket.logs.bucket
  description = "Name of the bucket that receives access logs."
}
