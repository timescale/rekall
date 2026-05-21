output "public_ip" {
  description = "Public IP of the benchmark instance"
  value       = aws_instance.benchmark.public_ip
}

output "instance_id" {
  description = "EC2 instance ID"
  value       = aws_instance.benchmark.id
}

output "instance_type" {
  description = "EC2 instance type"
  value       = aws_instance.benchmark.instance_type
}

output "pg_port" {
  description = "PostgreSQL port"
  value       = var.pg_port
}

output "dsn" {
  description = "PostgreSQL connection string"
  value       = "host=${aws_instance.benchmark.public_ip} port=${var.pg_port} dbname=rekall user=rekall password=rekall sslmode=disable"
}

output "iam_instance_profile" {
  description = "IAM instance profile name"
  value       = var.iam_instance_profile
}
